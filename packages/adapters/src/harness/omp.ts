import { AcpAdapterBase } from './acp-base';
import type { NativePermissionHost } from './native/host';

/**
 * Genuine OMP (oh-my-pi) ACP adapter — `omp acp` over stdio, JSON-RPC 2.0.
 *
 * Verified against the installed CLI (18.8.9): `omp acp` runs an Agent Client
 * Protocol server over stdio and `omp --version` prints `omp/18.8.9`.
 *
 * Isolation: OMP resolves auth, sessions, settings and caches from its own
 * home. The worker gets a managed HOME plus XDG roots under the state dir, and
 * a dedicated OMP profile name, so a worker session can never read or write the
 * operator's own OMP auth/session state.
 */
export class OmpAdapter extends AcpAdapterBase {
  constructor(host: NativePermissionHost, options: { binary?: string; stateDir?: string; profile?: string } = {}) {
    const profile = options.profile ?? 'ironbark-worker';
    super(
      host,
      {
        harness: 'omp',
        binary: options.binary ?? 'omp',
        // Global flags precede the subcommand; `omp acp` itself takes none.
        spawnArgs: ['--profile', profile, 'acp'],
        versionArgs: ['--version'],
        versionPatterns: [/^omp\/(\d+\.\d+\.\d+[\w.-]*)/i, /(\d+\.\d+\.\d+[\w.-]*)/],
        env: (stateDir: string) => ({
          HOME: `${stateDir}/home`,
          XDG_CONFIG_HOME: `${stateDir}/xdg/config`,
          XDG_CACHE_HOME: `${stateDir}/xdg/cache`,
          XDG_DATA_HOME: `${stateDir}/xdg/data`,
          XDG_STATE_HOME: `${stateDir}/xdg/state`,
        }),
        stateDirName: 'omp-home',
      },
      { stateDir: options.stateDir },
    );
  }
}
