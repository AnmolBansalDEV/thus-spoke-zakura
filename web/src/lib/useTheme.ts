import { useCallback, useSyncExternalStore } from 'react';
import { applyTheme, readStoredTheme, THEME_STORAGE_KEY, type ThemePreference } from './theme';

const listeners = new Set<() => void>();

// The choice this tab made when storage would not take it, and whether storage
// has refused at all: a read that threw or a write that did not land. Where
// storage answers it stays the source of truth, so clearing the stored
// preference restores "system" instead of resurrecting this.
let selectedPreference: ThemePreference | null = null;
let storageRefused = false;

// Called from getSnapshot, so a refusal is recorded while React reads. That is
// deliberate and monotonic: once storage has refused, the selection this tab
// made is what the control shows for the rest of the page's life.
function readPreference(): ThemePreference {
  if (storageRefused) return selectedPreference ?? 'system';
  const stored = readStoredTheme();
  if (stored === null) {
    storageRefused = true;
    return selectedPreference ?? 'system';
  }
  return stored;
}

function emit() {
  for (const listener of listeners) listener();
}

function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  // Keep other tabs in step.
  window.addEventListener('storage', onChange);
  return () => {
    listeners.delete(onChange);
    window.removeEventListener('storage', onChange);
  };
}

/**
 * The theme preference, read through useSyncExternalStore for the same reason
 * as the motion preference: it lives outside React (the DOM attribute and
 * localStorage), so it is subscribed to rather than mirrored into state. When
 * storage refuses to answer, the snapshot is the selection this tab made
 * instead, so the control and the page agree on it.
 */
export function useTheme(): [ThemePreference, (next: ThemePreference) => void] {
  const preference = useSyncExternalStore(subscribe, readPreference, () => 'system' as const);

  const setPreference = useCallback((next: ThemePreference) => {
    selectedPreference = next;
    try {
      localStorage.setItem(THEME_STORAGE_KEY, next);
    } catch {
      // Storage will not persist it: this tab keeps the choice until reload.
      storageRefused = true;
    }
    applyTheme(next);
    emit();
  }, []);

  return [preference, setPreference];
}
