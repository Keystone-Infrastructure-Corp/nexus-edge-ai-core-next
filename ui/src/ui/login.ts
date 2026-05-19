// M6 Phase 2 Step 2.9 — full-screen login overlay.
//
// Rendered into a dedicated `#auth-overlay` host outside the
// app grid when `auth.mode in {local, oidc, hybrid}` AND
// there's no valid session. Self-removes once login succeeds
// AND any follow-up `force_password_reset` modal resolves.
//
// Three surface variants, driven by the engine's
// `GET /v1/auth/info` probe:
//
//  * `mode = local` — username + password form only.
//  * `mode = oidc`  — "Sign in with <provider>" button only.
//  * `mode = hybrid` — both, with the username+password form
//    primary and the OIDC button rendered below a divider.
//
// Posture choices, deliberate:
//
// * The overlay is the only DOM surface visible until login
//   finishes. We do NOT render the sidebar/main shell "behind"
//   the form (no peek-through, no clickjacking surface).
// * Generic error text. The engine returns the same
//   `invalid_credentials` shape for unknown-user, wrong-password,
//   locked, and disabled — we mirror that opacity in the UI.
// * Submit button stays disabled while the request is in
//   flight to prevent the double-submit race that would burn
//   two lockout slots.
// * The OIDC button is rendered iff `info.allows_oidc` AND
//   `info.oidc_display_name` is non-null — the engine only
//   surfaces the display-name when discovery succeeded, so
//   the button never points at a 404.

import { auth as authApi } from "../api/auth.js";
import {
  consumeOidcError,
  getAuthInfo,
  getSession,
  sessionFromTokenResponse,
  setSession,
} from "../lib/auth.js";
import { mountForcePasswordResetModal } from "./change-password-modal.js";

const HOST_ID = "auth-overlay";

/// Mount the login overlay. Idempotent — if already mounted,
/// re-uses the existing host. `onComplete` fires once a valid
/// non-force-reset session is in place; the caller uses that
/// signal to mount the app shell.
export function mountLoginOverlay(onComplete: () => void): void {
  const host = ensureHost();
  while (host.firstChild) host.removeChild(host.firstChild);
  host.style.display = "flex";

  // The auth-info probe was already populated by `loadAuthInfo`
  // at boot; default conservatively (both surfaces on) if it's
  // somehow null so the operator still has a path through.
  const info = getAuthInfo();
  const mode = info?.mode ?? "local";
  const showLocalForm = mode === "local" || mode === "hybrid";
  const showOidcButton =
    (info?.allows_oidc ?? false) && (info?.oidc_display_name ?? null) != null;
  const oidcLabel = info?.oidc_display_name ?? "single sign-on";

  const card = document.createElement("div");
  card.className = "auth-card";

  const brand = document.createElement("div");
  brand.className = "auth-brand";
  brand.textContent = "Nexus Edge AI";
  card.appendChild(brand);

  const subtitle = document.createElement("div");
  subtitle.className = "auth-subtitle";
  subtitle.textContent = "Sign in to continue";
  card.appendChild(subtitle);

  // M6 Phase 3 Step 3.3 UI — surface `?oidc_error=<code>` from
  // the callback redirect exactly once, then strip it from the
  // address bar so a refresh doesn't re-show it. We render it
  // ABOVE the form so the user notices before retrying.
  const oidcErr = consumeOidcError();
  if (oidcErr) {
    const banner = document.createElement("div");
    banner.className = "auth-error";
    banner.setAttribute("role", "alert");
    banner.textContent = formatOidcError(oidcErr);
    card.appendChild(banner);
  }

  if (showLocalForm) {
    card.appendChild(buildLocalForm(host, onComplete));
  }

  if (showOidcButton) {
    if (showLocalForm) {
      // Divider only when we're showing both surfaces.
      const divider = document.createElement("div");
      divider.className = "auth-divider";
      divider.textContent = "or";
      card.appendChild(divider);
    }
    card.appendChild(buildOidcButton(oidcLabel));
  }

  host.appendChild(card);

  // Focus the first form field for keyboard-first ergonomics
  // when the local form is on screen; otherwise focus the
  // OIDC button so Enter triggers it.
  setTimeout(() => {
    const firstInput = card.querySelector<HTMLInputElement>("input[name=username]");
    if (firstInput) {
      firstInput.focus();
      return;
    }
    const oidcBtn = card.querySelector<HTMLButtonElement>(".auth-oidc-btn");
    if (oidcBtn) oidcBtn.focus();
  }, 0);
}

function buildLocalForm(host: HTMLElement, onComplete: () => void): HTMLFormElement {
  const form = document.createElement("form");
  form.className = "auth-form";
  form.autocomplete = "on";

  const userLabel = document.createElement("label");
  userLabel.className = "auth-label";
  userLabel.textContent = "Username";
  const userInput = document.createElement("input");
  userInput.className = "auth-input";
  userInput.type = "text";
  userInput.name = "username";
  userInput.autocomplete = "username";
  userInput.required = true;
  userInput.spellcheck = false;
  userInput.autocapitalize = "off";
  userLabel.appendChild(userInput);

  const passLabel = document.createElement("label");
  passLabel.className = "auth-label";
  passLabel.textContent = "Password";
  const passInput = document.createElement("input");
  passInput.className = "auth-input";
  passInput.type = "password";
  passInput.name = "password";
  passInput.autocomplete = "current-password";
  passInput.required = true;
  passLabel.appendChild(passInput);

  const submit = document.createElement("button");
  submit.className = "auth-submit";
  submit.type = "submit";
  submit.textContent = "Sign in";

  const error = document.createElement("div");
  error.className = "auth-error";
  error.setAttribute("role", "alert");
  error.style.display = "none";

  form.appendChild(userLabel);
  form.appendChild(passLabel);
  form.appendChild(error);
  form.appendChild(submit);

  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const username = userInput.value.trim();
    const password = passInput.value;
    if (!username || !password) return;

    submit.disabled = true;
    submit.textContent = "Signing in…";
    error.style.display = "none";

    authApi
      .login({ username, password })
      .then((tok) => {
        const session = sessionFromTokenResponse(tok);
        setSession(session);
        if (session.user.force_password_reset) {
          // Hand off to the force-reset modal. The overlay
          // stays mounted underneath so the user can't escape
          // into the shell until they pick a new password.
          mountForcePasswordResetModal(session, () => {
            host.style.display = "none";
            onComplete();
          });
        } else {
          host.style.display = "none";
          onComplete();
        }
      })
      .catch((e: unknown) => {
        const msg =
          e instanceof Error && /^401/.test(e.message)
            ? "Invalid username or password."
            : e instanceof Error && /^4\d\d/.test(e.message)
              ? "Sign in failed. Check your credentials and try again."
              : "Sign in failed. Try again in a moment.";
        error.textContent = msg;
        error.style.display = "block";
        passInput.value = "";
        passInput.focus();
      })
      .finally(() => {
        submit.disabled = false;
        submit.textContent = "Sign in";
      });
  });

  return form;
}

function buildOidcButton(label: string): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "auth-oidc-btn";
  btn.textContent = `Sign in with ${label}`;
  btn.addEventListener("click", () => {
    btn.disabled = true;
    btn.textContent = "Redirecting…";
    // We always hand the IdP back to `/` — hash routes are
    // restored after the shell mounts. Sending the current
    // hash would round-trip through the IdP's URL filter and
    // is unnecessary since the SPA's last-route memory lives
    // in `localStorage` already.
    authApi
      .oidcStart({ redirect_to: "/" })
      .then((res) => {
        // Full-page navigation — the engine relies on the
        // browser following the IdP's 302 back to
        // `/api/v1/auth/oidc/callback`, so SPA-internal
        // routing (history.pushState) is the wrong tool.
        window.location.assign(res.authorization_url);
      })
      .catch((e: unknown) => {
        btn.disabled = false;
        btn.textContent = `Sign in with ${label}`;
        // Surface the failure in the existing error banner
        // if one is already rendered; otherwise create one.
        let err = btn.parentElement?.querySelector<HTMLDivElement>(
          ".auth-error.auth-error-oidc",
        );
        if (!err) {
          err = document.createElement("div");
          err.className = "auth-error auth-error-oidc";
          err.setAttribute("role", "alert");
          btn.parentElement?.insertBefore(err, btn);
        }
        err.textContent =
          e instanceof Error && /^5\d\d/.test(e.message)
            ? "Single sign-on is temporarily unavailable. Try again in a moment."
            : "Couldn't start single sign-on. Try again.";
      });
  });
  return btn;
}

function formatOidcError(code: string): string {
  switch (code) {
    case "access_denied":
      return "Sign-in cancelled.";
    case "unmapped_role":
      return "Your account doesn't have access to this system. Contact an administrator.";
    case "oidc_user_not_eligible":
      return "Your account is disabled. Contact an administrator.";
    case "idp_token_endpoint":
    case "idp_invalid_token":
      return "The identity provider rejected this sign-in. Try again.";
    case "auth_not_configured":
      return "Single sign-on isn't configured on this engine.";
    case "bad_request":
      return "Sign-in link expired. Click the button to start again.";
    default:
      return `Single sign-on failed (${code}). Try again.`;
  }
}

/// Hide the login overlay (e.g. after the caller mounted the
/// shell themselves on a pre-existing session). Does NOT
/// remove the host element — the next `mountLoginOverlay` call
/// reuses it.
export function hideLoginOverlay(): void {
  const host = document.getElementById(HOST_ID);
  if (host) host.style.display = "none";
}

/// Re-show the overlay (e.g. after logout). Equivalent to
/// `mountLoginOverlay` but spelled to communicate intent at
/// the call site.
export function showLoginOverlay(onComplete: () => void): void {
  mountLoginOverlay(onComplete);
}

/// True iff a valid (non-force-reset) session is in place. The
/// boot sequence uses this to decide whether to skip straight
/// to the shell or render the overlay.
export function hasUsableSession(): boolean {
  const s = getSession();
  return s != null && !s.user.force_password_reset;
}

function ensureHost(): HTMLElement {
  let host = document.getElementById(HOST_ID);
  if (!host) {
    host = document.createElement("div");
    host.id = HOST_ID;
    host.className = "auth-overlay";
    document.body.appendChild(host);
  }
  return host;
}
