import type { IGitforestAPI } from '../main/preload';

declare global {
  interface Window {
    gf: IGitforestAPI;
  }
}

export {};
