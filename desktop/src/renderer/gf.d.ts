import type { IGitForestAPI } from '../main/preload';

declare global {
  interface Window {
    gf: IGitForestAPI;
  }
}

export {};
