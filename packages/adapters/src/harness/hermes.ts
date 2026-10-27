import { AcpAdapterBase } from './acp-base';
import type { NativePermissionHost } from './native/host';

/** Genuine Hermes ACP adapter (hermes acp over stdio, JSON-RPC 2.0).
 *  Profile isolation via HERMES_HOME (verified in hermes_cli/config.py env allowlist). */
export class HermesAdapter extends AcpAdapterBase {
  constructor(host: NativePermissionHost, options: { binary?: string; stateDir?: string } = {}) {
    super(host, {
      harness: 'hermes',
      binary: options.binary ?? 'hermes',
      spawnArgs: ['acp'],
      versionArgs: ['--version'],
      versionPatterns: [/Hermes Agent v([\d.]+)/i, /hermes\s+v?(\d+\.\d+\.\d+[\w.-]*)/i],
      env: (stateDir: string) => ({
        HERMES_HOME: stateDir,
        // Hermes logs to its own file logs; silence console log spam on the RPC channel.
        HERMES_LOG_LEVEL: 'ERROR',
      }),
      stateDirName: 'hermes-home',
    }, { stateDir: options.stateDir });
  }
}
