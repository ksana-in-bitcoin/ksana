import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import './styles/tokens.css';
import './styles/base.css';
import './styles/app.css';
import './app/theme';
import { App } from './app/App';
import { connect, connectionStore } from './chain/connection';
import { loadDeployment } from './chain/deployment';
import { checkWalletFor, connectWallet, lastWallet } from './chain/wallet';
import { currentConnection } from './chain/connection';
import { localeStore } from './i18n';

localeStore.subscribe(() => {
  document.documentElement.lang = localeStore.get();
});

let tried = false;
connectionStore.subscribe(() => {
  const conn = currentConnection();
  checkWalletFor(conn);
  // a wallet connected on an earlier visit comes back without asking again (Unisat and OneKey answer without a prompt
  // when the page is already allowed; Xverse always asks, so it waits for the button)
  if (conn && !tried && lastWallet() === 'unisat') {
    tried = true;
    void connectWallet('unisat', conn).catch(() => {});
  }
});

void loadDeployment().then(() => {
  void connect();
  const root = document.getElementById('root');
  if (root) createRoot(root).render(<StrictMode><App /></StrictMode>);
});
