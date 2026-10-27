import { AcpAdapterBase } from './acp-base';
import type { NativePermissionHost } from './native/host';

/** Genuine OpenCode ACP adapter (opencode acp over stdio, JSON-RPC 2.0). */
export class OpenCodeAdapter extends AcpAdapterBase {
  constructor(host: NativePermissionHost, options: { binary?: string; stateDir?: string } = {}) {
    super(host, {
      harness: 'opencode',
      binary: options.binary ?? 'opencode',
      spawnArgs: ['acp'],
      versionArgs: ['--version'],
      versionPatterns: [/^opencode\s+v?([\d.]+)/i, /opencode v?(\d+\.\d+\.\d+[\w.-]*)/i, /^v?(\d+\.\d+\.\d+[\w.-]*)$/],
      env: (stateDir: string) => ({
        // OpenCode reads XDG config/cache/state; point them at the managed state dir so
        // worker sessions and config never touch the operator's ~/.config/opencode.
        XDG_CONFIG_HOME: `${stateDir}/xdg/config`,
        XDG_CACHE_HOME: `${stateDir}/xdg/cache`,
        XDG_DATA_HOME: `${stateDir}/xdg/data`,
      }),
      stateDirName: 'opencode-home',
    }, { stateDir: options.stateDir });
  }
}
