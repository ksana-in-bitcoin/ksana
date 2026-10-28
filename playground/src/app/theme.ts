/** Light, dark, or the system's choice; index.html applies the saved choice before the first paint. */
import { createStore, loadPref, savePref, useStore } from './store';

export type Theme = 'system' | 'light' | 'dark';

const isTheme = (v: unknown): v is Theme => v === 'system' || v === 'light' || v === 'dark';

export const themeStore = createStore<Theme>(loadPref<Theme>('theme', 'system', isTheme));

function apply(theme: Theme) {
  if (typeof document === 'undefined') return;
  if (theme === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.setAttribute('data-theme', theme);
}

apply(themeStore.get());

export function setTheme(theme: Theme): void {
  savePref('theme', theme === 'system' ? null : theme);
  themeStore.set(theme);
  apply(theme);
}

export function useTheme(): Theme {
  return useStore(themeStore);
}
