<script lang="ts">
import { getCurrentWindow } from '@tauri-apps/api/window';
import { onMount } from 'svelte';

onMount(() => {
	const currentWindow = getCurrentWindow();
	let unlisten: (() => void) | undefined;

	const apply = (theme: string | null) => {
		const dark = theme === 'dark';
		document.documentElement.classList.toggle('dark', dark);
		document.documentElement.style.colorScheme = dark ? 'dark' : 'light';
	};

	void (async () => {
		try {
			// WebView2 does not reliably propagate the OS theme into the CSS
			// prefers-color-scheme media query, so the window theme — which
			// Tauri tracks correctly (the titlebar follows it) — is the source
			// of truth. The media-query pass in app.html remains as a
			// first-paint approximation for hosts where it does work.
			apply(await currentWindow.theme());
			unlisten = await currentWindow.onThemeChanged(({ payload }) => apply(payload));
		} catch (error) {
			console.error('Unable to follow the window theme; media query only.', error);
		}
	})();

	return () => unlisten?.();
});
</script>

<slot />
