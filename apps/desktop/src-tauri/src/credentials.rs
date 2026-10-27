//! Native Secret Service credential dialog (plan D3).
//!
//! `open_credential_dialog` is the ONLY path a raw secret enters or leaves this
//! crate. GTK password entry runs on a dedicated glib main-context thread so the
//! webview's event loop is never blocked inside GTK's nested loop, and the raw
//! value is handed to `secret-tool` over an in-process pipe: it never appears in
//! the process argv, in Tauri invoke arguments or results, in logs, or in the
//! event stream. The reply is the same `AuthRef` the caller presented — a
//! pointer, never a secret.
//!
//! Resolution order per `AuthRef` kind:
//! - `secret_service { service, account }` — GTK dialog -> `secret-tool store`
//!   under `ironbark-assistant/<service>/<account>` (label "Ironbark Assistant").
//! - `bitwarden { item_id, field }` — owner's `bw get item` CLI; the field is
//!   read from the JSON output. No dialog: the Bitwarden session is the
//!   owner's own, selected explicitly at registration.
//! - `native_session { harness, profile }` — already resolved by the harness
//!   itself (its own profile login state); nothing to resolve, nothing to store.
//!
//! A cancelled dialog is the typed error `credential_cancelled`; a missing `bw`
//! binary or `secret-tool` failure surfaces the real tool exit, never a green
//! success.

use std::process::Stdio;
use std::sync::mpsc;

use ironbark_core::contracts::AuthRef;

/// The single native credential entry point. Blocking on purpose: a password
/// dialog is a human round-trip, and the webview UI freezing while the owner
/// types is the honest behaviour (the dialog is its own top-level window).
/// The raw secret never appears in the invoke arguments or result — only the
/// caller's own `AuthRef` comes back.
#[tauri::command]
pub fn open_credential_dialog(reference: AuthRef) -> Result<AuthRef, String> {
    match &reference {
        AuthRef::SecretService { service, account } => {
            let value = interactive_secret_service()?;
            store_secret_tool(service, account, &value)?;
        }
        AuthRef::Bitwarden { item_id, field } => {
            let _ = bitwarden_secret(item_id, field)?;
        }
        // The harness owns this credential (its own profile login state).
        AuthRef::NativeSession { .. } => {}
    }
    Ok(reference)
}

/// One GTK password prompt. Returns the raw secret (in memory only) or the
/// typed cancellation error.
fn interactive_secret_service() -> Result<String, String> {
    let title = "Ironbark Assistant — store credential in Secret Service";
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    std::thread::Builder::new()
        .name("ironbark-credential".to_owned())
        .spawn(move || {
            let context = glib::MainContext::new();
            context.invoke(move || {
                let result = run_dialog(&title);
                let _ = tx.send(result);
            });
            context.wait();
        })
        .map_err(|e| format!("credential thread: {e}"))?;
    rx.recv().map_err(|e| format!("credential dialog channel: {e}"))?
}

/// Build the password dialog and pump its modal loop on THIS thread (the glib
/// invoke thread created by `interactive_secret_service`). The password entry
/// is a real `gtk::Entry` with visibility off; its value is read in-process
/// and wiped from the widget with `set_text("")` before the dialog closes.
fn run_dialog(title: &str) -> Result<String, String> {
    let gtk_init = gtk::init().map_err(|e| format!("gtk_init: {e:?}"))?;
    let dialog = gtk::Dialog::new();
    dialog.set_title(title);
    dialog.set_default_size(440, 180);
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Save", gtk::ResponseType::Ok);
    let content = dialog.content_area();
    let box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    box.set_margin_start(16);
    box.set_margin_end(16);
    box.set_margin_top(16);
    box.set_margin_bottom(16);
    box.set_spacing(8);
    content.append(&box);
    let prompt = gtk::Label::new(Some("Enter the credential. It is stored in your Secret Service and never shown again."));
    prompt.set_xalign(0.0);
    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_hexpand(true);
    box.append(&prompt);
    box.append(&entry);
    box.show_all();
    dialog.present();
    let response = dialog.run();
    let value = entry.text().to_string();
    entry.set_text("");
    dialog.response(response);
    dialog.destroy();
    drop(gtk_init);
    if response != gtk::ResponseType::Ok {
        return Err("credential_cancelled".to_owned());
    }
    if value.is_empty() {
        return Err("credential_empty".to_owned());
    }
    Ok(value)
}

/// Resolve a Bitwarden field through the owner's installed `bw` CLI and the
/// session the owner selected. The raw field value crosses only an in-process
/// stdout pipe.
fn bitwarden_secret(item_id: &str, field: &str) -> Result<String, String> {
    let output = std::process::Command::new("bw")
        .args(["get", "item", item_id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("bitwarden_missing: bw CLI not available ({e})"))?;
    if !output.status.success() {
        return Err("bitwarden_unavailable".to_owned());
    }
    let item: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("bitwarden_parse: {e}"))?;
    let value = item
        .pointer("/fields")
        .and_then(|fields| fields.as_array())
        .and_then(|fields| fields.iter().find(|f| f.get("name").and_then(|n| n.as_str()) == Some(field)))
        .and_then(|f| f.pointer("/value"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "bitwarden_field_missing".to_owned())?;
    if value.is_empty() {
        return Err("bitwarden_field_empty".to_owned());
    }
    Ok(value)
}

/// Store the raw secret in the system Secret Service via `secret-tool store`.
/// The secret crosses only an in-process stdin pipe (never argv); the label is
/// the stable "Ironbark Assistant" and the lookup attributes are the same
/// `{service, account}` the core stores in its `AuthRef` — so a later read is
/// an exact attribute lookup, not a name guess.
fn store_secret_tool(service: &str, account: &str, value: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Command;
    let mut child = Command::new("secret-tool")
        .args(["store", "--label=Ironbark Assistant"])
        .args(["ironbark-assistant", "ironbark-assistant"])
        .args(["service", service])
        .args(["account", account])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("secret_tool_missing: secret-tool not available ({e})"))?;
    child.stdin.take().ok_or("stdin pipe")?.write_all(value.as_bytes()).map_err(|e| format!("secret_tool_write: {e}"))?;
    let output = child.wait_with_output().map_err(|e| format!("secret_tool_wait: {e}"))?;
    if !output.status.success() {
        return Err(format!("secret_tool_error: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}
