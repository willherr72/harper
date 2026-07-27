use std::num::NonZeroU32;
use std::sync::Arc;

#[cfg(target_os = "windows")]
use egui_wgpu::WgpuSetupCreateNew;
use egui_wgpu::winit::Painter;
use egui_wgpu::{RendererOptions, WgpuConfiguration, WgpuSetup};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Window as WinitWindow, WindowButtons, WindowId, WindowLevel};

use super::Error;
use super::render_state::{RectTransform, RenderState};

/// Marks the overlay as never-activating via `WS_EX_NOACTIVATE`.
///
/// The overlay must receive clicks — highlight and popup interaction — without
/// ever becoming the active window. Activation would pull the foreground away
/// from the app being linted, which both drops that app out of the per-app
/// allowlist and disturbs the focused text element mid-interaction: the lint
/// popup vanished on the very click meant to open it. winit's
/// `with_active(false)` only suppresses the initial activation, and winit
/// rewrites `GWL_EXSTYLE` from its own flag cache on every cursor-hittest
/// toggle, so this must be applied at creation *and* after every toggle.
#[cfg(target_os = "windows")]
fn apply_no_activate(window: &WinitWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_NOACTIVATE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if let Ok(handle) = window.window_handle()
        && let RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        let hwnd = HWND(win32.hwnd.get() as _);
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
        }
    }
}

/// A transparent click-through overlay window for one monitor.
///
/// `Window` owns the native winit window plus egui/wgpu integration required to render into it. It
/// deliberately does not own highlighter drawing decisions; those are supplied by `RenderState`
/// during each redraw.
pub struct Window {
    inner: Arc<WinitWindow>,
    egui_state: egui_winit::State,
    painter: Painter,
    viewport_id: egui::ViewportId,
}

impl Window {
    pub async fn new(
        event_loop: &ActiveEventLoop,
        monitor: MonitorHandle,
        context: egui::Context,
    ) -> Result<Self, Error> {
        let position = monitor.position();
        let size = monitor.size();
        let attributes = WinitWindow::default_attributes()
            .with_title("Harper")
            .with_inner_size(size)
            .with_position(position)
            .with_resizable(false)
            .with_enabled_buttons(WindowButtons::empty())
            .with_decorations(false)
            .with_transparent(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_active(false);

        // Without WS_EX_NOREDIRECTIONBITMAP a borderless window keeps its opaque
        // GDI redirection surface, which DWM composites as white and hides the
        // transparent DirectComposition-visual swapchain the overlay renders
        // into (see the wgpu setup below). Decorated windows don't need this,
        // but the overlay is deliberately borderless.
        #[cfg(target_os = "windows")]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes
                .with_no_redirection_bitmap(true)
                // The per-monitor overlays are not applications the user
                // switches to; without this each shows up as a blank taskbar
                // button. macOS avoids the equivalent via its Accessory
                // activation policy.
                .with_skip_taskbar(true)
        };

        let window = Arc::new(event_loop.create_window(attributes)?);

        #[cfg(target_os = "windows")]
        apply_no_activate(&window);

        window.set_outer_position(PhysicalPosition::new(position.x, position.y));
        let _ = window.request_inner_size(PhysicalSize::new(size.width, size.height));
        window.set_cursor_hittest(false)?;
        let viewport_id = egui::ViewportId::from_hash_of(window.id());

        let egui_state = egui_winit::State::new(
            context.clone(),
            viewport_id,
            event_loop,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );

        // On Windows the default DXGI swapchain is created from the window's
        // HWND, and HWND flip-model swapchains only support an opaque
        // `CompositeAlphaMode` — so the transparent overlay renders opaque.
        // Selecting the DX12 backend with a DirectComposition-visual swapchain
        // (`DxgiFromVisual`) exposes premultiplied alpha, which egui-wgpu then
        // uses to composite the overlay transparently. Other platforms keep the
        // default setup: Metal supports transparency already, and forcing DX12
        // there would be invalid.
        #[cfg(target_os = "windows")]
        let wgpu_setup = {
            let mut setup =
                WgpuSetupCreateNew::from_display_handle(event_loop.owned_display_handle());
            setup.instance_descriptor.backends = egui_wgpu::wgpu::Backends::DX12;
            setup
                .instance_descriptor
                .backend_options
                .dx12
                .presentation_system = egui_wgpu::wgpu::Dx12SwapchainKind::DxgiFromVisual;
            WgpuSetup::CreateNew(setup)
        };
        #[cfg(not(target_os = "windows"))]
        let wgpu_setup = WgpuSetup::from_display_handle(event_loop.owned_display_handle());

        let mut painter = Painter::new(
            context,
            WgpuConfiguration {
                wgpu_setup,
                ..Default::default()
            },
            true,
            RendererOptions::default(),
        )
        .await;
        painter
            .set_window(viewport_id, Some(window.clone()))
            .await?;
        window.request_redraw();

        Ok(Self {
            inner: window,
            egui_state,
            painter,
            viewport_id,
        })
    }

    /// Whether the OS reports this window as dark-themed.
    #[cfg(target_os = "windows")]
    pub fn is_dark(&self) -> bool {
        matches!(self.inner.theme(), Some(winit::window::Theme::Dark))
    }

    pub fn id(&self) -> WindowId {
        self.inner.id()
    }

    pub fn request_redraw(&self) {
        self.inner.request_redraw();
    }

    /// Controls whether the transparent overlay can receive pointer events.
    ///
    /// Highlight windows are click-through by default so the underlying app remains usable. The
    /// window manager temporarily enables hit-testing when global cursor polling shows the pointer
    /// is over an interactive highlight or popup.
    pub fn set_cursor_hittest(&self, enabled: bool) -> Result<(), Error> {
        self.inner.set_cursor_hittest(enabled)?;

        // winit rebuilds the whole extended style from its internal flag cache
        // on every hittest toggle, wiping externally applied bits — so the
        // no-activate style must be re-asserted here or the first hover would
        // silently remove it.
        #[cfg(target_os = "windows")]
        apply_no_activate(&self.inner);

        Ok(())
    }

    pub fn handle_event(&mut self, event: &WindowEvent) {
        let response = self.egui_state.on_window_event(&self.inner, event);

        if response.repaint {
            self.inner.request_redraw();
        }

        if let WindowEvent::Resized(size) = event
            && let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            self.painter
                .on_window_resized(self.viewport_id, width, height);
            self.inner.request_redraw();
        }
    }

    /// Display scale for popup hit-testing when the cursor is over this window.
    ///
    /// Returns `None` when the position is outside the window. On non-Windows
    /// platforms broker coordinates are already in points, so the scale is 1.0.
    pub fn popup_scale_at(&self, pos: egui::Pos2) -> Option<f32> {
        let origin = self.inner.outer_position().unwrap_or_default();
        let size = self.inner.inner_size();
        let inside = pos.x >= origin.x as f32
            && pos.x < (origin.x + size.width as i32) as f32
            && pos.y >= origin.y as f32
            && pos.y < (origin.y + size.height as i32) as f32;

        if !inside {
            return None;
        }

        #[cfg(target_os = "windows")]
        {
            Some(self.inner.scale_factor() as f32)
        }
        #[cfg(not(target_os = "windows"))]
        {
            Some(1.0)
        }
    }

    /// Transform from the broker's global coordinate space into this window's
    /// local egui points.
    ///
    /// On Windows the broker reports physical screen pixels, so the draw path
    /// must subtract this window's origin and divide by its scale factor.
    /// Elsewhere the identity transform preserves the existing 1:1 mapping.
    fn rect_transform(&self) -> RectTransform {
        #[cfg(target_os = "windows")]
        {
            let origin = self.inner.outer_position().unwrap_or_default();
            RectTransform {
                offset: egui::vec2(origin.x as f32, origin.y as f32),
                scale: self.inner.scale_factor() as f32,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            RectTransform::identity()
        }
    }

    pub fn render(&mut self, render_state: &mut RenderState) {
        let transform = self.rect_transform();
        let context = self.egui_state.egui_ctx().clone();
        let input = self.egui_state.take_egui_input(&self.inner);
        let output = context.run_ui(input, |ui| {
            render_state.render(ui, transform);
        });

        self.egui_state
            .handle_platform_output(&self.inner, output.platform_output);

        let clipped_primitives = context.tessellate(output.shapes, output.pixels_per_point);
        self.painter.paint_and_update_textures(
            self.viewport_id,
            output.pixels_per_point,
            [0.0, 0.0, 0.0, 0.0],
            &clipped_primitives,
            &output.textures_delta,
            Vec::new(),
        );
    }
}
