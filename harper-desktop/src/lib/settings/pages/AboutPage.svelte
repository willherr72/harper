<script lang="ts">
import { openUrl } from '@tauri-apps/plugin-opener';
import { onMount } from 'svelte';
import { Client } from '$lib/client';
import { DesktopUpdater } from '$lib/DesktopUpdater';

const PLATFORM_LABELS: Record<string, string> = {
	macos: 'Mac',
	windows: 'Windows',
	linux: 'Linux',
};

const SOURCE_URL = 'https://github.com/Automattic/harper';
const ISSUE_URL = 'https://github.com/Automattic/harper/issues/new/choose';

let currentVersion = '';
let platformLabel = '';

onMount(() => {
	void loadCurrentVersion();
	void loadPlatformLabel();
});

async function loadPlatformLabel() {
	try {
		platformLabel = PLATFORM_LABELS[await Client.platform()] ?? '';
	} catch (error) {
		console.error('Unable to determine platform.', error);
	}
}

async function loadCurrentVersion() {
	try {
		currentVersion = await DesktopUpdater.getCurrentVersion();
	} catch (error) {
		console.error('Unable to load Harper Desktop version.', error);
	}
}
</script>

<section class="about">
        <div class="about-mark">H</div>
        <h1>Harper{platformLabel ? ` for ${platformLabel}` : ''}</h1>
        <p class="muted">Version {currentVersion || 'unknown'}</p>
        <p>
          An open-source grammar checker that runs entirely on your device. No accounts, no
          telemetry, no cloud.
        </p>
        <div class="actions-row center">
          <button
            class="button"
            type="button"
            on:click={() => void openUrl('https://github.com/Automattic/harper/releases/latest')}
          >Release notes</button>
          <button class="button" type="button" on:click={() => void openUrl(SOURCE_URL)}>Source on GitHub</button>
          <button class="button" type="button" on:click={() => void openUrl(ISSUE_URL)}>Report an issue</button>
        </div>
        <div class="about-footer">
          Harper is free software released under the Apache 2.0 license.
          <br />
          Copyright 2026 The Harper Contributors.
        </div>
      </section>
