%define strongopts 1
Name:           ironbark-assistant
Version:        0.1.0
Release:        1%{?dist}
Summary:        Standalone workstation assistant: Rust core, CLI and namespace bridge
License:        MIT
URL:            https://github.com/mdyerapis-coder/ironbark-assistant
Source0:        https://github.com/mdyerapis-coder/ironbark-assistant/archive/refs/tags/v%{version}.tar.gz

# Portable macro fallbacks. These are defined on Fedora/EL; when building or
# verifying on a non-EL rpm (e.g. Arch's rpmbuild) they are undefined, so
# define equivalent paths to keep the spec buildable everywhere.
%{!?_docdir: %define _docdir %{_datadir}/doc}
%{!?_licensedir: %define _licensedir %{_datadir}/licenses}
# desktop-file-utils may be absent in a minimal buildroot; provide harmless
# fallbacks so the %post/%postun desktop hooks never break installation.
%{!?_desktop_database_install: %define _desktop_database_install :}
%{!?_desktop_database_uninstall: %define _desktop_database_uninstall :}
%{!?_gtk_install_icon_cache: %define _gtk_install_icon_cache :}
%{!?_gtk_uninstall_icon_cache: %define _gtk_uninstall_icon_cache :}

# The Rust workspace is built from source. SQLite is bundled by the rusqlite
# "bundled" feature, so no system sqlite is a hard build dependency.
BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  glibc-devel
BuildRequires:  desktop-file-utils
# The desktop client is a Tauri application and links WebKitGTK/JavaScriptCore.
BuildRequires:  webkit2gtk4.1-devel
BuildRequires:  javascriptcoregtk4.1-devel
BuildRequires:  librsvg2-devel
BuildRequires:  openssl-devel

# bubblewrap is required by the worker-containment boundary; glibc is the
# dynamic runtime. bun is only needed to run the provider/JEV adapter worker.
Requires:       bubblewrap
Recommends:     bun

%description
Ironbark Assistant is a standalone workstation assistant. A single Rust core
owns durable state (SQLite), an authenticated local Unix socket, the policy
engine, the service dispatcher and the worker/network containment boundary.
Bun adapter workers carry provider and JEV transports and never become a
second scheduler.

This package ships:
  /usr/bin/ironbark-core         daemon + namespace runner
  /usr/bin/ironbark-cli          operator CLI (one JSON DTO per subcommand)
  /usr/bin/ironbark-desktop      the headless desktop application (Tauri wrapper)
  /opt/ironbark/namespace-bridge private network bridge, staged at this exact
                                 path inside worker namespaces
  a headless desktop entry       the installed surface is the core daemon;
                                 there is no GUI yet

Run scheduling and crash reconciliation (C4) are implemented and covered by
tests and the crash-recovery smoke; live provider/harness launch, the judgment
domains and the desktop release are not yet qualified. See docs/runtime/ in the
source for the exact verified boundary.

%prep
%setup -q -n ironbark-assistant-%{version}

%build
# The cc crate (libsqlite3-sys "bundled") compiles sqlite3.c honoring
# CFLAGS/CXXFLAGS; with distro LTO (-flto) those become LTO bytecode
# objects the rustc/rust-lld final link cannot resolve. Strip LTO for
# this build so the bundled C compiles to plain objects.
export CFLAGS="$(echo "${CFLAGS:-}" | sed -E 's/(^|[[:space:]])-flto(=[A-Za-z0-9_-]+)?//g')"
export CXXFLAGS="$(echo "${CXXFLAGS:-}" | sed -E 's/(^|[[:space:]])-flto(=[A-Za-z0-9_-]+)?//g')"
    cargo build --release -p ironbark-core -p ironbark-cli -p ironbark-desktop

%install
rm -rf %{buildroot}
release_dir="${CARGO_TARGET_DIR:-target}/release"
install -Dm755 "${release_dir}/ironbark-core"    %{buildroot}%{_bindir}/ironbark-core
install -Dm755 "${release_dir}/ironbark-cli"     %{buildroot}%{_bindir}/ironbark-cli
install -Dm755 "${release_dir}/ironbark-desktop" %{buildroot}%{_bindir}/ironbark-desktop
install -Dm755 "${release_dir}/namespace_bridge" %{buildroot}/opt/ironbark/namespace-bridge
install -Dm644 LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm644 README.md %{buildroot}%{_docdir}/%{name}/README.md
install -Dm644 packaging/desktop/ironbark-assistant.desktop %{buildroot}%{_datadir}/applications/ironbark-assistant.desktop
install -Dm644 packaging/desktop/ironbark-assistant.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/ironbark-assistant.svg

%post
%{_desktop_database_install}
%{_gtk_install_icon_cache}

%postun
%{_desktop_database_uninstall}
%{_gtk_uninstall_icon_cache}

%files
%license %{_licensedir}/%{name}/LICENSE
%doc %{_docdir}/%{name}/README.md
%{_bindir}/ironbark-core
%{_bindir}/ironbark-cli
%{_bindir}/ironbark-desktop
/opt/ironbark/namespace-bridge
%{_datadir}/applications/ironbark-assistant.desktop
%{_datadir}/icons/hicolor/scalable/apps/ironbark-assistant.svg

%changelog
* Tue Oct 13 2026 Mason <ironbark@llmclouds.au> - 0.1.0-1
- Initial RPM packaging: core, CLI, namespace bridge and a headless desktop
  entry, built from the v0.1.0 source tag.
- Install the CLI binary the %files list already claimed, own the staged
  namespace bridge at /opt/ironbark, and add the WebKitGTK build requirements
  the Tauri desktop client needs.
