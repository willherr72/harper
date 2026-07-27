use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::monitor::MonitorHandle;
#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

use super::AddToDictionary;
use super::DisableRule;
use super::Error;
use super::IgnoreLint;
use super::RefreshConfig;
use super::render_state::{HitTarget, RenderState};
use super::window::Window;
use crate::os_broker::{LintText, OsBroker};
use crate::rect::ActionableLint;

const DEFAULT_READ_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Owns the winit event loop and the overlay windows created for each monitor.
///
/// `WindowManager` is intentionally separate from `Highlighter` because winit event-loop ownership
/// is a process-level concern. It also keeps monitor enumeration, native window lifecycle, and event
/// dispatch out of the public highlighter API.
pub struct WindowManager {
    event_loop: EventLoop<()>,
    context: egui::Context,
    rects: Vec<ActionableLint>,
    os_broker: Box<dyn OsBroker>,
    lint_text: LintText,
    ignore_lint: IgnoreLint,
    add_to_dictionary: AddToDictionary,
    disable_rule: DisableRule,
    refresh_config: RefreshConfig,
}

pub struct WindowManagerCallbacks {
    pub lint_text: LintText,
    pub ignore_lint: IgnoreLint,
    pub add_to_dictionary: AddToDictionary,
    pub disable_rule: DisableRule,
    pub refresh_config: RefreshConfig,
}

impl WindowManager {
    /// Creates the event loop before windows exist because winit requires window creation to happen
    /// from inside that loop's lifecycle callbacks.
    pub fn new(
        context: egui::Context,
        os_broker: Box<dyn OsBroker>,
        callbacks: WindowManagerCallbacks,
    ) -> Result<Self, Error> {
        let mut event_loop_builder = EventLoop::builder();

        #[cfg(target_os = "macos")]
        event_loop_builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_default_menu(false);

        Ok(Self {
            event_loop: event_loop_builder.build()?,
            context,
            rects: Vec::new(),
            os_broker,
            lint_text: callbacks.lint_text,
            ignore_lint: callbacks.ignore_lint,
            add_to_dictionary: callbacks.add_to_dictionary,
            disable_rule: callbacks.disable_rule,
            refresh_config: callbacks.refresh_config,
        })
    }

    /// Seeds externally supplied lint rectangles before the event loop takes ownership of rendering.
    pub fn set_rects(&mut self, rects: Vec<ActionableLint>) {
        self.rects = rects;
    }

    /// Transfers ownership into winit's application handler because `run_app` owns the process-level
    /// event loop until the overlay exits.
    pub fn run_window_for_each_monitor(self) -> Result<(), Error> {
        let mut app = WindowManagerApp::new(
            self.context,
            self.rects,
            self.os_broker,
            WindowManagerCallbacks {
                lint_text: self.lint_text,
                ignore_lint: self.ignore_lint,
                add_to_dictionary: self.add_to_dictionary,
                disable_rule: self.disable_rule,
                refresh_config: self.refresh_config,
            },
        );

        self.event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + DEFAULT_READ_INTERVAL,
        ));
        let result = self.event_loop.run_app(&mut app);

        if let Some(error) = app.error {
            return Err(error);
        }

        result.map_err(Error::from)
    }
}

struct WindowManagerApp {
    /// Shared egui context handed to each overlay window.
    ///
    /// On Windows each window instead gets its own context (see `resumed`), so
    /// this field is unused there.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    context: egui::Context,
    windows: Vec<Window>,
    render_state: RenderState,
    os_broker: Box<dyn OsBroker>,
    lint_text: LintText,
    read_interval: Duration,
    last_read: Instant,
    last_config_poll: Instant,
    refresh_config: RefreshConfig,
    hovered_lint: Option<usize>,
    cursor_hittest_enabled: bool,
    /// Counts poll ticks so the expensive accessibility read can run on a
    /// divider of the render cadence. Used on Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    read_tick: u32,
    /// Hash of the last scene handed to the renderer, plus a dirty flag, so
    /// identical frames are not re-rendered. Used on Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    scene_signature: u64,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    scene_dirty: bool,
    /// Popup visibility on the previous tick, to schedule the erase frame when
    /// the card closes. Used on Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    popup_was_open: bool,
    /// Last unconditional repaint. The change-driven render skip means a GPU
    /// surface invalidated by display sleep would otherwise stay blank until
    /// the scene next changes; a low-frequency heartbeat restores the
    /// self-healing the old every-tick render provided. Used on Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    last_heartbeat: Instant,
    /// Last event-loop tick, used to detect system suspend: the loop cannot
    /// tick through sleep, so a large gap means the machine resumed and the
    /// overlay surfaces need rebinding. Used on Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    last_tick: Instant,
    error: Option<Error>,
}

/// On Windows the accessibility read runs on every Nth poll tick (~10 Hz at a
/// 60 Hz display) instead of every tick. Walking UIA — focused element, text,
/// one sub-range per lint — is cross-process COM and dominated the process's
/// CPU at 60 Hz, while lint freshness is already bounded by the lint debounce.
/// Cursor hit-testing and rendering stay at full tick rate.
#[cfg(target_os = "windows")]
const READ_TICK_DIVIDER: u32 = 6;

impl WindowManagerApp {
    /// Builds the mutable application state consumed by winit callbacks after `WindowManager` gives
    /// up direct control of the event loop.
    fn new(
        context: egui::Context,
        rects: Vec<ActionableLint>,
        os_broker: Box<dyn OsBroker>,
        callbacks: WindowManagerCallbacks,
    ) -> Self {
        let read_interval = DEFAULT_READ_INTERVAL;
        Self {
            context,
            windows: Vec::new(),
            render_state: RenderState::new(
                rects,
                callbacks.ignore_lint,
                callbacks.add_to_dictionary,
                callbacks.disable_rule,
            ),
            os_broker,
            lint_text: callbacks.lint_text,
            read_interval,
            last_read: Instant::now() - read_interval,
            last_config_poll: Instant::now(),
            refresh_config: callbacks.refresh_config,
            hovered_lint: None,
            cursor_hittest_enabled: false,
            read_tick: 0,
            scene_signature: 0,
            scene_dirty: true,
            popup_was_open: false,
            last_heartbeat: Instant::now(),
            last_tick: Instant::now(),
            error: None,
        }
    }

    /// Order-sensitive hash of the lint geometry the renderer would draw.
    ///
    /// Two reads that produce the same rectangles for the same lint kinds
    /// produce the same signature, letting the render loop skip repainting an
    /// unchanged scene.
    #[cfg(target_os = "windows")]
    fn scene_signature_of(rects: &[ActionableLint]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        for lint in rects {
            lint.rect.x.to_bits().hash(&mut hasher);
            lint.rect.y.to_bits().hash(&mut hasher);
            lint.rect.width.to_bits().hash(&mut hasher);
            lint.rect.height.to_bits().hash(&mut hasher);
            std::mem::discriminant(&lint.lint.lint_kind).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Refreshes lint geometry from the OS broker inside the event loop so repaint requests happen on
    /// the same thread that owns the overlay windows.
    fn read_rect_updates(&mut self) {
        // Windows: divide the accessibility read down from the render cadence,
        // reusing the previous rectangles between reads.
        #[cfg(target_os = "windows")]
        let read_now = {
            let due = self.read_tick == 0;
            self.read_tick = (self.read_tick + 1) % READ_TICK_DIVIDER;
            due
        };
        #[cfg(not(target_os = "windows"))]
        let read_now = true;

        if read_now {
            let rects = self.os_broker.get_boxes(self.lint_text.as_mut());

            #[cfg(target_os = "windows")]
            {
                let signature = Self::scene_signature_of(&rects);
                if signature != self.scene_signature {
                    self.scene_signature = signature;
                    self.scene_dirty = true;
                }
            }

            self.render_state.set_rects(rects);
        }

        // Windows: render directly instead of round-tripping through
        // request_redraw. winit delivers RedrawRequested via WM_PAINT, which
        // the OS only synthesizes when the thread's message queue is idle —
        // and this event loop never idles (this poll re-marks every window
        // dirty each tick), so paint events starve and only one of the
        // per-monitor windows ever repaints. Direct rendering drives all of
        // them deterministically. The RedrawRequested path still handles
        // OS-initiated repaints such as resizes.
        //
        // Identical frames are skipped: with an unchanged scene and no popup
        // open there is nothing new to draw, and repainting three 4K overlay
        // surfaces per tick was the dominant idle cost. An open popup forces
        // continuous rendering because egui widgets animate on hover.
        #[cfg(target_os = "windows")]
        {
            // popup_was_open covers the erase frame: the click that closes the
            // card mutates popup state *during* that frame's render, so one
            // more paint is needed afterwards to clear the card from screen.
            let popup_open = self.render_state.popup_open();

            if self.last_heartbeat.elapsed() >= Duration::from_secs(5) {
                self.last_heartbeat = Instant::now();
                self.scene_dirty = true;
            }

            let render_needed = self.scene_dirty || popup_open || self.popup_was_open;
            if render_needed {
                let dark = self.windows.first().is_some_and(Window::is_dark);
                self.render_state.set_dark(dark);
                for window in &mut self.windows {
                    window.render(&mut self.render_state);
                }
                self.scene_dirty = false;
            }
            self.popup_was_open = self.render_state.popup_open();
        }

        #[cfg(not(target_os = "windows"))]
        for window in &self.windows {
            window.request_redraw();
        }
    }

    /// Builds one overlay window per monitor, replacing whatever is there.
    ///
    /// Shared by first-run setup and by resume recovery so a rebuilt overlay is
    /// constructed exactly like the original.
    fn create_windows(&mut self, event_loop: &ActiveEventLoop) {
        let monitors = event_loop.available_monitors().collect::<Vec<_>>();

        if let Some(refresh_interval) = monitors.iter().filter_map(monitor_refresh_interval).min() {
            self.read_interval = refresh_interval;
        }

        for monitor in monitors {
            // Each overlay window gets its own egui context on Windows. Sharing
            // one context across the per-monitor windows makes egui's texture
            // deltas race between their independent wgpu renderers: the initial
            // font-atlas allocation reaches only the first window to render, so
            // a later atlas update delivered to another window panics in
            // egui-wgpu with "Tried to update a texture that has not been
            // allocated yet." Independent contexts route each window's texture
            // deltas to its own renderer. macOS keeps the shared context.
            #[cfg(target_os = "windows")]
            let context = egui::Context::default();
            #[cfg(not(target_os = "windows"))]
            let context = self.context.clone();

            match pollster::block_on(Window::new(event_loop, monitor, context)) {
                Ok(window) => self.windows.push(window),
                Err(error) => {
                    self.error = Some(error);
                    event_loop.exit();
                    return;
                }
            }
        }
    }

    fn refresh_config(&mut self) {
        (self.refresh_config)();
    }

    /// Display scale used for popup hit-testing at a cursor position: the scale
    /// of whichever overlay window contains it, or 1.0 when none does — which
    /// also keeps non-Windows platforms at their existing unscaled behavior.
    fn popup_scale_at(&self, pos: egui::Pos2) -> f32 {
        self.windows
            .iter()
            .find_map(|window| window.popup_scale_at(pos))
            .unwrap_or(1.0)
    }

    /// Toggles native click-through behavior based on the cursor's current interactive Harper target,
    /// keeping editor clicks from being swallowed when the pointer is outside highlights and popups.
    fn update_cursor_hittest(&mut self, event_loop: &ActiveEventLoop) {
        let Some(cursor_pos) = self.os_broker.cursor_position() else {
            return;
        };

        let popup_scale = self.popup_scale_at(cursor_pos);
        let hit_target = self.render_state.hit_target_at_pos(cursor_pos, popup_scale);
        self.hovered_lint = match hit_target {
            HitTarget::Lint(index) => Some(index),
            HitTarget::Popup | HitTarget::None => None,
        };

        let should_enable_hittest = !matches!(hit_target, HitTarget::None);

        if self.cursor_hittest_enabled == should_enable_hittest {
            return;
        }

        for window in &self.windows {
            if let Err(error) = window.set_cursor_hittest(should_enable_hittest) {
                self.error = Some(error);
                event_loop.exit();
                return;
            }
        }

        self.cursor_hittest_enabled = should_enable_hittest;
    }

    /// Opens or replaces the popup only when the actual click target is a lint, leaving popup clicks
    /// for egui controls such as the close button.
    fn select_hit_lint(&mut self) {
        let Some(cursor_pos) = self.os_broker.cursor_position() else {
            return;
        };

        let popup_scale = self.popup_scale_at(cursor_pos);
        let HitTarget::Lint(index) = self.render_state.hit_target_at_pos(cursor_pos, popup_scale)
        else {
            return;
        };

        self.render_state.set_highlighted_lint(Some(index));

        for window in &self.windows {
            window.request_redraw();
        }
    }
}

fn monitor_refresh_interval(monitor: &MonitorHandle) -> Option<Duration> {
    monitor
        .refresh_rate_millihertz()
        .filter(|refresh_rate| *refresh_rate > 0)
        .map(|refresh_rate| Duration::from_nanos(1_000_000_000_000 / u64::from(refresh_rate)))
}

impl ApplicationHandler for WindowManagerApp {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        // A gap this large means the event loop was frozen — i.e. the system
        // was suspended. The DirectComposition visuals behind the overlay
        // surfaces can detach across resume without reporting any error: every
        // later render presents successfully to nothing.
        //
        // Recreating just the surface does not help. `Painter::set_window` is
        // a no-op once a surface is registered for the viewport, so an earlier
        // attempt to rebind silently did nothing at all, and even clearing the
        // surface first would leave the original instance and device in place.
        // The overlay windows are therefore rebuilt outright — cheap at once
        // per resume, and demonstrably fresh in every layer: HWND, instance,
        // device, swapchain and composition visual.
        #[cfg(target_os = "windows")]
        {
            if now.duration_since(self.last_tick) > Duration::from_secs(30) {
                // warn, not info: the app's subscriber is capped at WARN, and a
                // silent recovery event is exactly what made the previous
                // failure so expensive to diagnose.
                tracing::warn!("resume detected; recreating overlay windows");
                self.windows.clear();
                self.create_windows(event_loop);
                self.scene_dirty = true;
            }
            self.last_tick = now;
        }

        if now.duration_since(self.last_read) >= self.read_interval {
            self.read_rect_updates();
            self.last_read = now;
        }

        if now.duration_since(self.last_config_poll) >= CONFIG_POLL_INTERVAL {
            self.refresh_config();
            self.last_config_poll = now;
        }

        self.update_cursor_hittest(event_loop);

        let next_read = self.last_read + self.read_interval;
        let next_config_poll = self.last_config_poll + CONFIG_POLL_INTERVAL;
        event_loop.set_control_flow(ControlFlow::WaitUntil(next_read.min(next_config_poll)));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }

        self.create_windows(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let should_select_hit_lint = matches!(
            &event,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            }
        );
        let should_render = matches!(&event, WindowEvent::RedrawRequested);

        // Display wake, occlusion change, and focus change can all follow a
        // period in which the surface was invalidated; repaint promptly rather
        // than waiting for the heartbeat.
        #[cfg(target_os = "windows")]
        if matches!(
            &event,
            WindowEvent::Occluded(_) | WindowEvent::Focused(_) | WindowEvent::Moved(_)
        ) {
            self.scene_dirty = true;
        }

        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.id() == window_id)
        {
            // Windows: do not feed RedrawRequested back into egui. egui answers
            // any processed event with a repaint request, which handle_event
            // turns into another request_redraw — a self-sustaining paint loop
            // (~50 fps per window, forever) that was the entire idle CPU cost.
            // Rendering still happens below; interaction events still reach
            // egui and still trigger their own repaints.
            #[cfg(target_os = "windows")]
            if !should_render {
                window.handle_event(&event);
            }
            #[cfg(not(target_os = "windows"))]
            window.handle_event(&event);

            if should_render {
                window.render(&mut self.render_state);
            }
        }

        if should_select_hit_lint {
            self.select_hit_lint();
        }

        if matches!(&event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }
}
