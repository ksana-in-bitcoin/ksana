/**
 * The playground's text in English (the source language: its structure is the `Messages` type) and Simplified Chinese, one file
 * per part under `en/` and `zh-CN/`: the frame and the shared panels (`core`) and each example's page. A language with a message
 * missing, extra or with other parameters fails the type check.
 *
 * Like the web page (D-115) the playground opens in English unless the address bar's `?lang=` or a choice made in the settings
 * says otherwise; the choice is kept in this browser. Numbers, IDs and amounts are written the same way in both languages.
 */
import { createStore, loadPref, savePref, useStore } from '../app/store';
import en from './en';
import zhCN from './zh-CN';

export type Messages = typeof en;
export type Locale = 'en' | 'zh-CN';

/** Each language with its name in itself and the mark of the top bar's button that switches to it */
export const LOCALES: readonly { code: Locale; name: string; mark: string }[] = [
  { code: 'en', name: 'English', mark: 'EN' },
  { code: 'zh-CN', name: '\u7b80\u4f53\u4e2d\u6587', mark: '\u4e2d' },
];

const PACKS: Record<Locale, Messages> = { en, 'zh-CN': zhCN };

function isLocale(value: unknown): value is Locale {
  return value === 'en' || value === 'zh-CN';
}

function initialLocale(): Locale {
  if (typeof window !== 'undefined') {
    const asked = new URLSearchParams(window.location.search).get('lang');
    if (asked && /^zh/i.test(asked)) return 'zh-CN';
    if (asked && /^en/i.test(asked)) return 'en';
  }
  return loadPref<Locale>('locale', 'en', isLocale);
}

export const localeStore = createStore<Locale>(initialLocale());

export function setLocale(locale: Locale): void {
  savePref('locale', locale);
  localeStore.set(locale);
}

export function currentLocale(): Locale {
  return localeStore.get();
}

/** Messages of the current language, for code outside components */
export function t(): Messages {
  return PACKS[localeStore.get()];
}

export function useT(): Messages {
  return PACKS[useStore(localeStore)];
}

export function useLocale(): Locale {
  return useStore(localeStore);
}
