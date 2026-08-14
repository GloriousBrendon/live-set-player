<script lang="ts">
	import { deviceStatus } from '$lib/stores/device';

	const level = $derived.by((): 'ok' | 'warning' | 'error' | 'unknown' => {
		const d = $deviceStatus;
		if (!d) return 'unknown';
		if (!d.configured_device) return 'error';
		if (!d.open) return 'error';
		if (d.error) return 'error';
		if (d.notice) return 'warning';
		return 'ok';
	});

	const message = $derived.by(() => {
		const d = $deviceStatus;
		if (!d) return 'Checking output device…';
		if (!d.configured_device) return 'No output device selected — pick one in Settings.';
		if (!d.open) return `Output device "${d.configured_device}" is not open.`;
		if (d.error === 1) return `Output device "${d.configured_device}" is no longer available.`;
		if (d.error) return `Output device error on "${d.configured_device}".`;
		if (d.notice) return d.notice;
		return `Output: ${d.configured_device}`;
	});
</script>

<div class="banner {level}">{message}</div>

<style>
	.banner {
		width: 100%;
		padding: 1.2vh 2vw;
		font-size: 2.2vh;
		font-weight: 700;
		text-align: center;
		box-sizing: border-box;
		letter-spacing: 0.02em;
	}
	.banner.error {
		background: #ff375f;
		color: #fff;
	}
	.banner.warning {
		background: #ffd60a;
		color: #1a1a1a;
	}
	.banner.unknown {
		background: #222;
		color: #888;
	}
	.banner.ok {
		background: transparent;
		color: #4d6b52;
		font-weight: 500;
	}
</style>
