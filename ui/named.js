"use strict";

// Optional named tunnels: a fixed hostname on the user's own Cloudflare domain instead of a
// random trycloudflare.com link. Loaded after app.js and uses its helpers ($, invoke, toast,
// tunnels, renderCard and the rest).

/** {signedIn, certPath, signingIn, loginUrl} from the cloudflare_account command */
let account = { signedIn: false, certPath: null, signingIn: false, loginUrl: null };
let loginCanceled = false;
let removingId = null;

// ---------------------------------------------------------------- account (Settings)

function renderAccount() {
  $("#account-detail").textContent = account.signedIn ? "Signed in" : account.signingIn ? "Signing in…" : "Not signed in";
  $("#account-path").textContent = account.signedIn ? account.certPath ?? "" : "";
  $("#account-hint").textContent = account.signedIn
    ? "Tunnels can use hostnames on the domain you picked when you signed in. To use another domain, delete this file and sign in again."
    : "Sign in to give a tunnel a fixed hostname on your own domain, like app.example.com. Random trycloudflare.com links don't need an account.";
  $("#account-login").hidden = account.signedIn || account.signingIn;
  $("#account-cancel").hidden = !account.signingIn;
  $("#account-pending").hidden = !account.signingIn;
  const link = $("#account-url");
  link.textContent = account.loginUrl ?? "Waiting for cloudflared…";
  link.href = account.loginUrl ?? "#";
  $("#domain-field").hidden = !account.signedIn;
}

async function refreshAccount() {
  try {
    account = await invoke("cloudflare_account");
  } catch (e) {
    $("#account-error").textContent = errText(e);
  }
  renderAccount();
}

async function signIn() {
  $("#account-error").textContent = "";
  loginCanceled = false;
  account = { ...account, signingIn: true, loginUrl: null };
  renderAccount();
  try {
    account = await invoke("cloudflare_login");
    if (account.signedIn) toast("Signed in to Cloudflare");
  } catch (e) {
    if (!loginCanceled) $("#account-error").textContent = errText(e);
    await refreshAccount();
    return;
  }
  renderAccount();
}

function cancelSignIn() {
  loginCanceled = true;
  invoke("cloudflare_login_cancel").catch((e) => toast(errText(e)));
}

function openLoginUrl(ev) {
  ev.preventDefault();
  if (account.loginUrl) invoke("open_url", { url: account.loginUrl }).catch((e) => toast(errText(e)));
}

// ---------------------------------------------------------------- composer

/** Adds a tunnel from the composer, on the user's domain when a hostname was entered. */
async function addTunnelFromComposer(name, target) {
  const input = $("#hostname");
  const hostname = $("#domain-field").hidden ? "" : input.value.trim();
  if (!hostname) return invoke("add_tunnel", { name, target });
  const view = await invoke("add_named_tunnel", { name, target, hostname });
  input.value = "";
  return view;
}

// ---------------------------------------------------------------- edit dialog

function fillLinkFields(t) {
  const named = Boolean(t.named);
  $("#edit-kind-quick").checked = !named;
  $("#edit-kind-named").checked = named;
  $("#edit-hostname").value = t.named?.hostname ?? "";
  renderLinkFields();
}

function renderLinkFields() {
  const named = $("#edit-kind-named").checked;
  $("#edit-hostname-field").hidden = !named;
  const t = tunnels.get(editingId);
  let hint = "";
  if (named && !account.signedIn) {
    hint = "Sign in to Cloudflare in Settings first. The hostname must be on the domain you pick when you sign in.";
  } else if (named && t?.named?.tunnelId) {
    hint = `Uses the tunnel ${t.named.tunnelName ?? t.named.tunnelId} in your Cloudflare account. A new hostname gets its DNS record the next time the tunnel starts.`;
  } else if (named) {
    hint = "On the first start, Cloudfairy creates a tunnel in your Cloudflare account and a DNS record for this hostname.";
  } else if (t?.named?.tunnelId) {
    hint = `The tunnel ${t.named.tunnelName ?? t.named.tunnelId} and its DNS record stay in your Cloudflare account.`;
  }
  const hintEl = $("#edit-hostname-hint");
  hintEl.textContent = hint;
  hintEl.hidden = !hint;
}

/** Saves the link choice before the rest of the edit dialog. Throws a message when it's invalid. */
async function saveLinkFields(id) {
  const t = tunnels.get(id);
  const named = $("#edit-kind-named").checked;
  const hostname = named ? $("#edit-hostname").value.trim() : "";
  if (named && !hostname) throw "Enter a hostname on your domain, like app.example.com.";
  if ((t?.named?.hostname ?? "") === hostname) return;
  await invoke("set_tunnel_hostname", { id, hostname: hostname || null });
}

// ---------------------------------------------------------------- removing

/** Asks whether to also delete a named tunnel from the Cloudflare account, then removes it. */
function removeNamedTunnel(id) {
  const t = tunnels.get(id);
  if (!t) return;
  removingId = id;
  $("#remove-named-text").textContent = `Remove ${t.name} from Cloudfairy?`;
  $("#remove-named-delete").checked = false;
  $("#remove-named-hint").textContent =
    `Deletes the tunnel ${t.named.tunnelName ?? t.named.tunnelId} and its credentials file. ` +
    `The DNS record for ${t.named.hostname} stays. Delete it in the Cloudflare dashboard.`;
  $("#remove-named-error").textContent = "";
  $("#remove-named-dialog").showModal();
}

async function confirmNamedRemove() {
  const id = removingId;
  const btn = $("#remove-named-confirm");
  const deleteFromAccount = $("#remove-named-delete").checked;
  btn.disabled = true;
  btn.textContent = deleteFromAccount ? "Deleting…" : "Removing…";
  try {
    await invoke("remove_named_tunnel", { id, deleteFromAccount });
    tunnels.delete(id);
    openLogs.delete(id);
    openRequests.delete(id);
    cardFor(id)?.remove();
    updateEmpty();
    $("#remove-named-dialog").close();
    if (deleteFromAccount) toast("Tunnel deleted from Cloudflare");
  } catch (e) {
    $("#remove-named-error").textContent = errText(e);
  } finally {
    btn.disabled = false;
    btn.textContent = "Remove";
  }
}

// ---------------------------------------------------------------- boot

(function initNamed() {
  $("#account-login").addEventListener("click", signIn);
  $("#account-cancel").addEventListener("click", cancelSignIn);
  $("#account-url").addEventListener("click", openLoginUrl);
  for (const r of $$('input[name="edit-kind"]')) r.addEventListener("change", renderLinkFields);
  $("#remove-named-confirm").addEventListener("click", confirmNamedRemove);
  listen("cloudflare-login", ({ payload }) => {
    account = { ...account, signingIn: true, loginUrl: payload.url };
    renderAccount();
  });
  refreshAccount();
})();
