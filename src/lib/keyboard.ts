// Window-level keyboard shortcuts for every transport action (docs/SPEC.md §9).
// In-app only, not OS-global -- a footswitch that presents as a HID keyboard is
// covered by this for free, per §9's own note.

import { get } from 'svelte/store';
import * as api from '$lib/api';
import { status } from '$lib/stores/status';
import { saveProject } from '$lib/stores/project';

const EDITABLE_TAGS = new Set(['INPUT', 'TEXTAREA', 'SELECT']);

function isEditingText(target: EventTarget | null): boolean {
	if (!(target instanceof HTMLElement)) return false;
	return EDITABLE_TAGS.has(target.tagName) || target.isContentEditable;
}

function handleKeydown(event: KeyboardEvent): void {
	if (isEditingText(event.target)) return;

	switch (event.code) {
		case 'Space': {
			event.preventDefault();
			const current = get(status);
			if (current?.state === 'playing') {
				api.stop().catch(() => {});
			} else {
				api.play().catch(() => {});
			}
			break;
		}
		case 'KeyS':
			if (event.ctrlKey || event.metaKey) {
				event.preventDefault();
				saveProject().catch(() => {});
			} else {
				event.preventDefault();
				api.stop().catch(() => {});
			}
			break;
		case 'Escape':
			event.preventDefault();
			api.panicStop().catch(() => {});
			break;
		case 'ArrowRight':
			event.preventDefault();
			api.advanceSection().catch(() => {});
			break;
		case 'Enter':
			event.preventDefault();
			api.armNextSong().catch(() => {});
			break;
		default:
			break;
	}
}

/** Install the shortcut listener; call the returned function to remove it. */
export function installKeyboardShortcuts(): () => void {
	window.addEventListener('keydown', handleKeydown);
	return () => window.removeEventListener('keydown', handleKeydown);
}
