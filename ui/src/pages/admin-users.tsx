// Admin Users page.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Copy,
  KeyRound,
  Lock,
  Plus,
  Trash2,
  Unlock,
  Users as UsersIcon,
} from "lucide-react";
import { useState } from "react";

import {
  createUser,
  deleteUser,
  listUsers,
  resetUserPassword,
  unlockUser,
  updateUser,
} from "@/api/admin";
import type { UserRole, UserView } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { formatAgo } from "@/lib/format";
import { MIN_PASSWORD_LEN } from "@/lib/password-policy";

export function AdminUsersPage() {
  const qc = useQueryClient();
  const [includeDeleted, setIncludeDeleted] = useState(false);
  const usersQuery = useQuery({
    queryKey: ["admin", "users", includeDeleted],
    queryFn: () => listUsers(includeDeleted),
  });

  const [createOpen, setCreateOpen] = useState(false);
  const [otpModal, setOtpModal] = useState<{
    title: string;
    username: string;
    otp: string;
  } | null>(null);

  const users = usersQuery.data?.users ?? [];

  const onUserMutated = () =>
    qc.invalidateQueries({ queryKey: ["admin", "users"] });

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Users</h1>
          <p className="text-sm text-muted-foreground">
            Local accounts. Role gates capability; deletion is soft and
            preserves audit history.
          </p>
        </div>
        <div className="flex items-center gap-3">
          <label className="inline-flex items-center gap-2 text-sm text-muted-foreground">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={includeDeleted}
              onChange={(e) => setIncludeDeleted(e.target.checked)}
            />
            Include deleted
          </label>
          <Button onClick={() => setCreateOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            New user
          </Button>
        </div>
      </header>

      <Card>
        <CardContent className="p-0">
          {usersQuery.isLoading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-10 w-full" />
              ))}
            </div>
          ) : users.length === 0 ? (
            <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
              <UsersIcon className="h-8 w-8 opacity-50" />
              <p>No users.</p>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="bg-muted/30 text-xs uppercase text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left">Username</th>
                    <th className="px-3 py-2 text-left">Role</th>
                    <th className="px-3 py-2 text-left">Status</th>
                    <th className="px-3 py-2 text-left">Last login</th>
                    <th className="px-3 py-2 text-left">Created</th>
                    <th className="px-3 py-2 text-right">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {users.map((u) => (
                    <UserRow
                      key={u.id}
                      user={u}
                      onMutated={onUserMutated}
                      onOtp={(otp) =>
                        setOtpModal({
                          title: "Password reset",
                          username: u.username,
                          otp,
                        })
                      }
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {createOpen ? (
        <CreateUserSheet
          onClose={() => setCreateOpen(false)}
          onCreated={(user, otp) => {
            setCreateOpen(false);
            onUserMutated();
            if (otp) {
              setOtpModal({
                title: "User created",
                username: user.username,
                otp,
              });
            }
          }}
        />
      ) : null}

      {otpModal ? (
        <OtpModal
          title={otpModal.title}
          username={otpModal.username}
          otp={otpModal.otp}
          onClose={() => setOtpModal(null)}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Row.
// ---------------------------------------------------------------------------

function UserRow({
  user,
  onMutated,
  onOtp,
}: {
  user: UserView;
  onMutated: () => void;
  onOtp: (otp: string) => void;
}) {
  const [error, setError] = useState<string | null>(null);

  const onError = (e: unknown) =>
    setError(e instanceof Error ? e.message : String(e));

  const updateMutation = useMutation({
    mutationFn: (req: { role?: UserRole; disabled?: boolean }) =>
      updateUser(user.id, req),
    onSuccess: () => {
      setError(null);
      onMutated();
    },
    onError,
  });

  const resetMutation = useMutation({
    mutationFn: () => resetUserPassword(user.id),
    onSuccess: (r) => {
      setError(null);
      onOtp(r.one_time_password);
      onMutated();
    },
    onError,
  });

  const unlockMutation = useMutation({
    mutationFn: () => unlockUser(user.id),
    onSuccess: () => {
      setError(null);
      onMutated();
    },
    onError,
  });

  const deleteMutation = useMutation({
    mutationFn: () => deleteUser(user.id),
    onSuccess: () => {
      setError(null);
      onMutated();
    },
    onError,
  });

  const isLocked =
    user.locked_until !== null && Date.parse(user.locked_until) > Date.now();
  const isDeleted = user.deleted_at !== null;

  return (
    <tr className="border-t border-border/40">
      <td className="px-3 py-2">
        <div className="font-medium">{user.username}</div>
        <div className="font-mono text-[10px] text-muted-foreground">
          {user.id}
        </div>
        {error ? (
          <div className="mt-1 text-xs text-destructive">{error}</div>
        ) : null}
      </td>
      <td className="px-3 py-2">
        <select
          className="h-7 rounded-md border border-input bg-transparent px-1.5 text-xs capitalize"
          value={user.role}
          disabled={
            updateMutation.isPending || isDeleted
          }
          onChange={(e) =>
            updateMutation.mutate({ role: e.target.value as UserRole })
          }
        >
          <option value="admin">admin</option>
          <option value="operator">operator</option>
          <option value="viewer">viewer</option>
        </select>
      </td>
      <td className="px-3 py-2">
        <StatusChips
          user={user}
          isLocked={isLocked}
          onToggleDisabled={() =>
            updateMutation.mutate({ disabled: !user.disabled })
          }
        />
      </td>
      <td className="px-3 py-2 text-xs text-muted-foreground">
        {user.last_login_at ? formatAgo(user.last_login_at) : "never"}
        {user.failed_login_count > 0 ? (
          <span className="ml-2 text-warning">
            ({user.failed_login_count} fail
            {user.failed_login_count === 1 ? "" : "s"})
          </span>
        ) : null}
      </td>
      <td className="px-3 py-2 text-xs text-muted-foreground">
        {formatAgo(user.created_at)}
      </td>
      <td className="px-3 py-2 text-right">
        <div className="flex justify-end gap-1">
          <Button
            size="sm"
            variant="ghost"
            title="Reset password"
            disabled={resetMutation.isPending || isDeleted}
            onClick={() => {
              if (
                confirm(
                  `Reset password for "${user.username}"? Active sessions will be revoked.`,
                )
              ) {
                resetMutation.mutate();
              }
            }}
          >
            <KeyRound className="h-4 w-4" />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            title={isLocked ? "Unlock" : "Already unlocked"}
            disabled={!isLocked || unlockMutation.isPending}
            onClick={() => unlockMutation.mutate()}
          >
            <Unlock className="h-4 w-4" />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            title="Delete"
            disabled={deleteMutation.isPending || isDeleted}
            onClick={() => {
              if (confirm(`Delete user "${user.username}"?`)) {
                deleteMutation.mutate();
              }
            }}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </td>
    </tr>
  );
}

function StatusChips({
  user,
  isLocked,
  onToggleDisabled,
}: {
  user: UserView;
  isLocked: boolean;
  onToggleDisabled: () => void;
}) {
  const chips: React.ReactNode[] = [];
  if (user.deleted_at) {
    chips.push(
      <Badge key="deleted" variant="destructive">
        deleted
      </Badge>,
    );
  }
  if (user.disabled) {
    chips.push(
      <Badge key="disabled" variant="secondary">
        disabled
      </Badge>,
    );
  } else if (!user.deleted_at) {
    chips.push(
      <Badge key="active" variant="success">
        active
      </Badge>,
    );
  }
  if (isLocked) {
    chips.push(
      <Badge key="locked" variant="warning">
        <Lock className="mr-1 h-3 w-3" />
        locked
      </Badge>,
    );
  }
  if (user.force_password_reset) {
    chips.push(
      <Badge key="force" variant="outline">
        force-reset
      </Badge>,
    );
  }
  if (user.has_oidc) {
    chips.push(
      <Badge key="oidc" variant="outline">
        oidc
      </Badge>,
    );
  }
  return (
    <div className="flex flex-wrap items-center gap-1">
      {chips}
      {!user.deleted_at ? (
        <button
          onClick={onToggleDisabled}
          className="text-[10px] text-muted-foreground underline-offset-2 hover:underline"
        >
          {user.disabled ? "enable" : "disable"}
        </button>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Create user sheet.
// ---------------------------------------------------------------------------

function CreateUserSheet({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (user: UserView, otp?: string) => void;
}) {
  const [username, setUsername] = useState("");
  const [role, setRole] = useState<UserRole>("viewer");
  const [setPassword, setSetPassword] = useState(false);
  const [password, setPassword2] = useState("");
  const [error, setError] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: () =>
      createUser({
        username: username.trim(),
        role,
        password: setPassword && password ? password : undefined,
      }),
    onSuccess: (r) => onCreated(r.user, r.one_time_password),
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!username.trim()) {
      setError("Username is required.");
      return;
    }
    if (setPassword && password.length < MIN_PASSWORD_LEN) {
      setError(`Password must be at least ${MIN_PASSWORD_LEN} characters.`);
      return;
    }
    mutation.mutate();
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title="New user"
      description="Engine generates a one-time password unless you supply one. force_password_reset is always set on create."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={mutation.isPending}>
            {mutation.isPending ? "Creating…" : "Create"}
          </Button>
        </>
      }
    >
      <form onSubmit={onSubmit}>
        {error ? (
          <div className="border-b border-destructive/50 bg-destructive/10 px-5 py-3 text-sm text-destructive">
            {error}
          </div>
        ) : null}
        <SheetSection title="Identity">
          <div className="space-y-2">
            <Label htmlFor="user-username">Username</Label>
            <Input
              id="user-username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoFocus
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="user-role">Role</Label>
            <select
              id="user-role"
              className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm capitalize"
              value={role}
              onChange={(e) => setRole(e.target.value as UserRole)}
            >
              <option value="admin">admin</option>
              <option value="operator">operator</option>
              <option value="viewer">viewer</option>
            </select>
          </div>
        </SheetSection>
        <SheetSection title="Password">
          <label className="inline-flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={setPassword}
              onChange={(e) => setSetPassword(e.target.checked)}
            />
            Set my own password
          </label>
          {setPassword ? (
            <div className="space-y-2">
              <Label htmlFor="user-pw">Password (min {MIN_PASSWORD_LEN} chars)</Label>
              <Input
                id="user-pw"
                type="password"
                value={password}
                onChange={(e) => setPassword2(e.target.value)}
              />
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              The engine will generate a 192-bit base64 OTP. It will be
              shown to you exactly once.
            </p>
          )}
        </SheetSection>
      </form>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// OTP reveal modal.
// ---------------------------------------------------------------------------

function OtpModal({
  title,
  username,
  otp,
  onClose,
}: {
  title: string;
  username: string;
  otp: string;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(otp);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // ignore
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-background/80 backdrop-blur-sm">
      <div
        className="w-full max-w-md rounded-lg border border-border bg-card p-5 shadow-lg"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-lg font-semibold">{title}</h2>
        <p className="mt-1 text-sm text-muted-foreground">
          Copy the one-time password for{" "}
          <span className="font-mono">{username}</span> now. It will{" "}
          <strong>not</strong> be shown again.
        </p>
        <div className="mt-4 flex items-center gap-2 rounded-md border border-border bg-muted/30 p-3">
          <code className="flex-1 break-all font-mono text-sm">{otp}</code>
          <Button size="sm" variant="outline" onClick={copy}>
            <Copy className="mr-1 h-3 w-3" />
            {copied ? "Copied" : "Copy"}
          </Button>
        </div>
        <div className="mt-4 flex justify-end">
          <Button onClick={onClose}>I've saved it</Button>
        </div>
      </div>
    </div>
  );
}
