// M6 Phase 2 Step 2.9c — /admin/users page.
//
// Top-down layout (single column, full main-pane width):
//
//   1. Header row — title, "Show deleted" toggle, "New user"
//      button.
//   2. Users table — id, username, role, status badges
//      (disabled / locked / force_reset / oidc), created_at,
//      and a kebab actions menu per row.
//   3. Modals (lazily created in <body>):
//      - new-user form (username, role, optional password)
//      - per-user OTP reveal (one-time, copy-to-clipboard)
//      - confirm-delete
//      - confirm-disable-self / confirm-demote-self when the
//        operator targets their own row (catches admin-foot-gun
//        before the engine's `last_admin` 409 trips)
//
// Authorisation: this page renders only when the current
// session principal has `role = admin` — `main.ts` injects the
// sidebar entry conditionally. If a non-admin somehow navigates
// to `#/admin-users` the API calls will return 403 and the
// table will render an empty + "you don't have permission"
// banner.

import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { getSession } from "../lib/auth.js";
import { toast } from "../lib/toast.js";
import { renderAuditHistory } from "./audit-history.js";
import type {
  CreateUserResponse,
  Role,
  UserView,
} from "../api/types.js";

const ROLES: Role[] = ["admin", "operator", "viewer"];

export async function renderAdminUsers(root: HTMLElement): Promise<void> {
  clear(root);

  const includeDeletedToggle = h("input", {
    type: "checkbox",
    id: "users-include-deleted",
  });
  const headerLabel = h(
    "label",
    { class: "users-toggle" },
    includeDeletedToggle,
    " Show deleted",
  );
  const newBtn = h(
    "button",
    { class: "primary", on: { click: () => openNewUserModal(reload) } },
    "New user",
  );

  const header = h(
    "div",
    { class: "users-header" },
    h("h2", null, "Users"),
    h("div", { class: "spacer" }),
    headerLabel,
    newBtn,
  );

  const tableHost = h("div", { class: "users-table-host" });
  root.append(header, tableHost);

  async function reload(): Promise<void> {
    await loadAndRenderTable(tableHost, includeDeletedToggle.checked, reload);
  }

  includeDeletedToggle.addEventListener("change", () => {
    void reload();
  });

  await reload();
}

async function loadAndRenderTable(
  host: HTMLElement,
  includeDeleted: boolean,
  reload: () => Promise<void>,
): Promise<void> {
  clear(host);
  const status = h("p", { class: "muted" }, "Loading users…");
  host.append(status);

  let users: UserView[];
  try {
    const resp = await api.adminUsers.list({ includeDeleted });
    users = resp.users;
  } catch (err) {
    status.textContent =
      err instanceof Error && /^403/.test(err.message)
        ? "You don't have permission to view users (admin only)."
        : `Failed to load users: ${
            err instanceof Error ? err.message : String(err)
          }`;
    return;
  }

  status.remove();
  if (users.length === 0) {
    host.append(
      h("p", { class: "muted" }, "No users to show."),
    );
    return;
  }

  // Sort: active first, then deleted at bottom; within each
  // group by created_at ascending so the bootstrap admin is row
  // 1 of a fresh install.
  users.sort((a, b) => {
    const ad = a.deleted_at ? 1 : 0;
    const bd = b.deleted_at ? 1 : 0;
    if (ad !== bd) return ad - bd;
    return a.created_at.localeCompare(b.created_at);
  });

  const table = h(
    "table",
    { class: "users-table" },
    h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", null, "ID"),
        h("th", null, "Username"),
        h("th", null, "Role"),
        h("th", null, "Status"),
        h("th", null, "Last login"),
        h("th", null, "Created"),
        h("th", { class: "right" }, "Actions"),
      ),
    ),
  );
  const tbody = h("tbody", null);
  for (const u of users) {
    tbody.append(renderRow(u, reload));
  }
  table.append(tbody);
  host.append(table);
}

function renderRow(u: UserView, reload: () => Promise<void>): HTMLElement {
  const session = getSession();
  const isSelf = session?.user.id === u.id;
  const isDeleted = u.deleted_at != null;
  const isLocked =
    u.locked_until != null && new Date(u.locked_until).getTime() > Date.now();

  // Status chips.
  const badges: HTMLElement[] = [];
  if (isDeleted) {
    badges.push(h("span", { class: "badge badge-muted" }, "deleted"));
  } else {
    if (u.disabled) {
      badges.push(h("span", { class: "badge badge-warn" }, "disabled"));
    }
    if (isLocked) {
      badges.push(h("span", { class: "badge badge-crit" }, "locked"));
    }
    if (u.force_password_reset) {
      badges.push(h("span", { class: "badge badge-info" }, "reset on next login"));
    }
    if (u.has_oidc) {
      badges.push(h("span", { class: "badge badge-muted" }, "oidc"));
    }
    if (badges.length === 0) {
      badges.push(h("span", { class: "badge badge-ok" }, "active"));
    }
  }

  // Actions.
  const actions = h("div", { class: "users-actions" });
  if (!isDeleted) {
    const roleSelect = h(
      "select",
      { class: "role-select" },
      ...ROLES.map((r) => h("option", { value: r }, r)),
    ) as HTMLSelectElement;
    roleSelect.value = u.role;
    roleSelect.addEventListener("change", () => {
      const newRole = roleSelect.value as Role;
      if (newRole === u.role) return;
      if (isSelf && newRole !== "admin") {
        if (!confirm(
          "You're about to demote your own admin role. You'll lose access to this page immediately. Continue?",
        )) {
          roleSelect.value = u.role;
          return;
        }
      }
      void api.adminUsers
        .update(u.id, { role: newRole })
        .then(() => {
          toast.success(`Role updated for ${u.username}.`);
          void reload();
        })
        .catch((e: unknown) => {
          const msg = e instanceof Error ? e.message : String(e);
          if (/last_admin/.test(msg)) {
            toast.error(
              "Can't change role — this is the only active admin.",
            );
          } else {
            toast.error(`Couldn't update role: ${msg}`);
          }
          roleSelect.value = u.role;
        });
    });
    actions.append(roleSelect);

    const disableBtn = h(
      "button",
      {
        class: "ghost",
        on: {
          click: () => {
            if (isSelf && !u.disabled) {
              if (!confirm(
                "You're about to disable your own account. You'll be signed out and unable to sign back in. Continue?",
              )) {
                return;
              }
            }
            void api.adminUsers
              .update(u.id, { disabled: !u.disabled })
              .then(() => {
                toast.success(
                  u.disabled
                    ? `Re-enabled ${u.username}.`
                    : `Disabled ${u.username}.`,
                );
                void reload();
              })
              .catch((e: unknown) => {
                const msg = e instanceof Error ? e.message : String(e);
                if (/last_admin/.test(msg)) {
                  toast.error(
                    "Can't disable — this is the only active admin.",
                  );
                } else {
                  toast.error(`Couldn't update user: ${msg}`);
                }
              });
          },
        },
      },
      u.disabled ? "Enable" : "Disable",
    );
    actions.append(disableBtn);

    if (isLocked) {
      const unlockBtn = h(
        "button",
        {
          class: "ghost",
          on: {
            click: () => {
              void api.adminUsers
                .unlock(u.id)
                .then(() => {
                  toast.success(`Unlocked ${u.username}.`);
                  void reload();
                })
                .catch((e: unknown) => {
                  toast.error(
                    `Couldn't unlock: ${
                      e instanceof Error ? e.message : String(e)
                    }`,
                  );
                });
            },
          },
        },
        "Unlock",
      );
      actions.append(unlockBtn);
    }

    const resetBtn = h(
      "button",
      {
        class: "ghost",
        on: {
          click: () => {
            if (!confirm(
              `Reset ${u.username}'s password? They'll be signed out and must use the new one-time password to log back in.`,
            )) {
              return;
            }
            void api.adminUsers
              .resetPassword(u.id)
              .then((resp) => {
                openOtpModal(u.username, resp.one_time_password);
                void reload();
              })
              .catch((e: unknown) => {
                toast.error(
                  `Couldn't reset password: ${
                    e instanceof Error ? e.message : String(e)
                  }`,
                );
              });
          },
        },
      },
      "Reset password",
    );
    actions.append(resetBtn);

    const deleteBtn = h(
      "button",
      {
        class: "ghost danger",
        on: {
          click: () => {
            if (isSelf) {
              if (!confirm(
                "You're about to delete your own account. You'll be signed out immediately. Continue?",
              )) {
                return;
              }
            } else if (!confirm(
              `Delete ${u.username}? Their audit history is preserved (soft delete); the username can be reused after.`,
            )) {
              return;
            }
            void api.adminUsers
              .remove(u.id)
              .then(() => {
                toast.success(`Deleted ${u.username}.`);
                void reload();
              })
              .catch((e: unknown) => {
                const msg = e instanceof Error ? e.message : String(e);
                if (/last_admin/.test(msg)) {
                  toast.error(
                    "Can't delete — this is the only active admin.",
                  );
                } else {
                  toast.error(`Couldn't delete: ${msg}`);
                }
              });
          },
        },
      },
      "Delete",
    );
    actions.append(deleteBtn);
  }

  // M6 Phase 4 Step 4.2 — always-visible "History" button (even
  // for soft-deleted users, since their audit trail is preserved).
  // Opens a modal with the per-user audit feed.
  const historyBtn = h(
    "button",
    {
      class: "ghost",
      on: { click: () => openHistoryModal(u) },
    },
    "History",
  );
  actions.append(historyBtn);

  // Tombstoned username (`<id>:deleted-<iso>`) is hard to read;
  // strip the prefix for display when the row is deleted.
  const displayName =
    isDeleted && u.username.startsWith(`${u.id}:deleted-`)
      ? "(deleted)"
      : u.username;

  return h(
    "tr",
    { class: isDeleted ? "row-deleted" : "" },
    h("td", null, String(u.id)),
    h("td", null, displayName + (isSelf ? " (you)" : "")),
    h("td", null, u.role),
    h("td", { class: "badges" }, ...badges),
    h("td", null, u.last_login_at ? formatTs(u.last_login_at) : "—"),
    h("td", null, formatTs(u.created_at)),
    h("td", { class: "right" }, actions),
  );
}

function formatTs(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleString();
  } catch {
    return iso;
  }
}

// ---------------------------------------------------------------------------
// Modals — created lazily in <body> so they outlive the tab
// rerender, dismissed by removing from the DOM.
// ---------------------------------------------------------------------------

function openNewUserModal(onCreated: () => Promise<void>): void {
  const backdrop = h("div", { class: "dialog-backdrop" });
  const dialog = h("div", { class: "dialog" });

  const title = h("h3", null, "Create user");
  const errorBanner = h("div", {
    class: "auth-error",
    style: { display: "none" },
  });

  const usernameInput = h("input", {
    type: "text",
    autocomplete: "off",
    spellcheck: false,
    class: "auth-input",
    required: true,
  }) as HTMLInputElement;
  const roleSelect = h(
    "select",
    { class: "auth-input" },
    ...ROLES.map((r) => h("option", { value: r }, r)),
  ) as HTMLSelectElement;
  roleSelect.value = "viewer";

  const passwordInput = h("input", {
    type: "password",
    autocomplete: "new-password",
    class: "auth-input",
    placeholder: "Leave blank to generate a one-time password",
  }) as HTMLInputElement;

  const form = h(
    "form",
    { class: "auth-form" },
    h("label", { class: "auth-label" }, "Username", usernameInput),
    h("label", { class: "auth-label" }, "Role", roleSelect),
    h(
      "label",
      { class: "auth-label" },
      "Initial password (optional)",
      passwordInput,
    ),
    errorBanner,
  );

  const submit = h(
    "button",
    { type: "submit", class: "primary" },
    "Create",
  ) as HTMLButtonElement;
  const cancel = h(
    "button",
    {
      type: "button",
      class: "ghost",
      on: { click: () => document.body.removeChild(backdrop) },
    },
    "Cancel",
  );
  const buttons = h(
    "div",
    { class: "dialog-footer" },
    cancel,
    submit,
  );
  form.append(buttons);

  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const username = usernameInput.value.trim();
    if (!username) {
      errorBanner.textContent = "Username is required.";
      errorBanner.style.display = "block";
      return;
    }
    const role = roleSelect.value as Role;
    const pw = passwordInput.value;
    submit.disabled = true;
    submit.textContent = "Creating…";
    errorBanner.style.display = "none";

    api.adminUsers
      .create({ username, role, ...(pw ? { password: pw } : {}) })
      .then((resp: CreateUserResponse) => {
        document.body.removeChild(backdrop);
        if (resp.one_time_password) {
          openOtpModal(resp.user.username, resp.one_time_password);
        } else {
          toast.success(`Created ${resp.user.username}.`);
        }
        void onCreated();
      })
      .catch((e: unknown) => {
        const msg = e instanceof Error ? e.message : String(e);
        // The engine returns stable error tags inside the body
        // — surface them as plain English.
        if (/username_taken/.test(msg)) {
          errorBanner.textContent = "That username is already in use.";
        } else if (/invalid_username/.test(msg)) {
          errorBanner.textContent =
            "Username must be 1–64 chars and can't contain colons, whitespace, or control characters.";
        } else if (/password_policy/.test(msg)) {
          const m = /password_policy: ([^"\\]+)/.exec(msg);
          errorBanner.textContent = m
            ? `Password rejected: ${m[1]}`
            : "Password rejected by policy (min 12 chars, not a common password).";
        } else {
          errorBanner.textContent = `Failed: ${msg}`;
        }
        errorBanner.style.display = "block";
      })
      .finally(() => {
        submit.disabled = false;
        submit.textContent = "Create";
      });
  });

  dialog.append(title, form);
  backdrop.append(dialog);
  document.body.append(backdrop);
  setTimeout(() => usernameInput.focus(), 0);
}

function openOtpModal(username: string, otp: string): void {
  const backdrop = h("div", { class: "dialog-backdrop" });
  const dialog = h("div", { class: "dialog" });
  dialog.append(h("h3", null, `One-time password for ${username}`));
  dialog.append(
    h(
      "p",
      { class: "muted" },
      "Copy this password now — it will not be shown again. The user must change it on next login.",
    ),
  );

  const otpField = h("input", {
    type: "text",
    readOnly: true,
    class: "otp-field",
    value: otp,
  }) as HTMLInputElement;

  const copyBtn = h(
    "button",
    {
      type: "button",
      class: "ghost",
      on: {
        click: () => {
          otpField.select();
          // Best-effort — modern browsers expose navigator.clipboard
          // but it requires a user-activation context (the click
          // handler satisfies that).
          void navigator.clipboard.writeText(otp).then(
            () => toast.success("Copied to clipboard."),
            () => {
              // Fall back to the legacy execCommand path — some
              // older Safari builds reject the modern API even
              // from a click context.
              try {
                document.execCommand("copy");
                toast.success("Copied to clipboard.");
              } catch {
                toast.info(
                  "Select and copy the password manually (Cmd/Ctrl+C).",
                );
              }
            },
          );
        },
      },
    },
    "Copy",
  );

  const done = h(
    "button",
    {
      type: "button",
      class: "primary",
      on: { click: () => document.body.removeChild(backdrop) },
    },
    "Done",
  );

  dialog.append(
    h("div", { class: "otp-row" }, otpField, copyBtn),
    h("div", { class: "dialog-footer" }, done),
  );
  backdrop.append(dialog);
  document.body.append(backdrop);
  setTimeout(() => {
    otpField.focus();
    otpField.select();
  }, 0);
}

// M6 Phase 4 Step 4.2 — per-user audit history modal. Opens
// `renderAuditHistory` inside a dialog backdrop so the operator
// can scan the user's recent activity without leaving the /admin/users
// page. The panel is pre-expanded since the whole point of opening
// this modal is to view the history.
function openHistoryModal(u: UserView): void {
  const backdrop = h("div", { class: "dialog-backdrop" });
  const dialog = h("div", { class: "dialog dialog-wide" });

  const title = h("h3", null, `Audit history — ${u.username}`);
  const panel = renderAuditHistory({
    resourceKind: "user",
    resourceId: String(u.id),
    open: true,
    limit: 100,
  });
  const close = h(
    "button",
    {
      type: "button",
      class: "primary",
      on: { click: () => document.body.removeChild(backdrop) },
    },
    "Close",
  );

  dialog.append(title, panel, h("div", { class: "dialog-footer" }, close));
  backdrop.append(dialog);
  document.body.append(backdrop);

  // ESC closes the modal.
  const onKey = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape") {
      document.removeEventListener("keydown", onKey);
      if (backdrop.parentNode === document.body) {
        document.body.removeChild(backdrop);
      }
    }
  };
  document.addEventListener("keydown", onKey);
}
