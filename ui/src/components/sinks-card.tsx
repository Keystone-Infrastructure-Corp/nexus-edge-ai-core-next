// Alert-sink configuration card for the local Delivery page.
//
// Until now the only way to configure alert delivery on a box was to
// hand-edit `nexus.toml` and restart, or to drive it from the cloud
// console. This card closes that gap for the on-site operator: it
// lists every effective sink (file + runtime), and lets an admin
// create, edit, test and remove the generic SMTP `email` sink without
// leaving the appliance.
//
// Scope note: only the `email` kind gets a full editor here. The
// webhook / SureView kinds are integrations that are provisioned
// centrally, so they render read-only (still testable, and still
// removable when runtime-managed). The cloud console carries the
// complete four-kind editor.
//
// Secret discipline: the engine redacts every secret in the GET
// response with `REDACTED_SECRET`; echoing the sentinel back on a PUT
// means "keep the stored secret". A brand-new sink must carry its
// secret in plaintext exactly once, on the create PUT.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertCircle,
  CheckCircle2,
  FileLock2,
  Mail,
  Plus,
  Send,
  Trash2,
} from "lucide-react";
import { useState } from "react";

import {
  REDACTED_SECRET,
  deleteSink,
  getSinks,
  putSink,
  testSink,
} from "@/api/storage";
import type {
  AdminSinkView,
  EmailSinkConfig,
  SinkConfig,
  TestSinkOut,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";

const KIND_LABEL: Record<string, string> = {
  email: "Email",
  webhook: "Webhook",
  sureview: "SureView Ops",
  sureview_email: "SureView Email",
};

/** Splits a pasted recipient list on commas, semicolons or newlines. */
function parseAddressList(raw: string): string[] {
  return raw
    .split(/[,;\n]/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

function formatAddressList(list: string[]): string {
  return list.join(", ");
}

/** "a@x, b@x" for ≤2 recipients, else "a@x +3 more". */
function recipientSummary(to: string[], cc: string[]): string {
  const all = [...to, ...cc];
  if (all.length === 0) return "no recipients";
  if (all.length <= 2) return all.join(", ");
  return `${all[0]} +${all.length - 1} more`;
}

function sinkSubtitle(cfg: SinkConfig): string {
  switch (cfg.kind) {
    case "email":
      return `${cfg.smtp_host} \u00b7 ${recipientSummary(cfg.to, cfg.cc)}`;
    case "webhook":
      return cfg.url;
    case "sureview":
      return `${cfg.region.toUpperCase()} \u00b7 ${cfg.system_identifier}`;
    case "sureview_email":
      return `${cfg.region.toUpperCase()} \u00b7 ${cfg.alarm_email}`;
  }
}

function blankEmailSink(): EmailSinkConfig {
  return {
    kind: "email",
    name: "",
    smtp_host: "",
    smtp_port: 587,
    starttls: true,
    from_address: "",
    from_name: null,
    to: [],
    cc: [],
    reply_to: null,
    subject_prefix: null,
    attach_snapshot: true,
    attach_clip: false,
    username: null,
    password: null,
    timeout_secs: 15,
  };
}

export function SinksCard() {
  const qc = useQueryClient();
  const sinksQuery = useQuery({ queryKey: ["delivery", "sinks"], queryFn: getSinks });
  const [editing, setEditing] = useState<EmailSinkConfig | null>(null);
  const [creating, setCreating] = useState(false);
  const [results, setResults] = useState<Record<string, TestSinkOut>>({});

  const sinks = sinksQuery.data?.sinks ?? [];
  const invalidate = () => {
    const refresh = () => {
      void qc.invalidateQueries({ queryKey: ["delivery", "sinks"] });
      void qc.invalidateQueries({ queryKey: ["delivery", "sinks-health"] });
    };
    refresh();
    // The engine rebuilds its live SinkRegistry asynchronously off the
    // `sink.config.changed` bus signal, so the `active` flag can still
    // be stale in the response to the refetch we just fired. One
    // follow-up settles it without polling forever.
    window.setTimeout(refresh, 750);
  };

  return (
    <Card>
      <CardContent className="space-y-3 p-5">
        <div className="flex items-center justify-between gap-3">
          <div>
            <h2 className="flex items-center gap-2 text-base font-semibold">
              <Send className="h-4 w-4" />
              Alert sinks
            </h2>
            <p className="text-xs text-muted-foreground">
              Where alerts go once the cascade decides to deliver them.
            </p>
          </div>
          <Button size="sm" onClick={() => setCreating(true)}>
            <Plus className="mr-1 h-4 w-4" />
            Add email sink
          </Button>
        </div>

        {sinksQuery.isLoading ? (
          <Skeleton className="h-24 w-full" />
        ) : sinksQuery.isError ? (
          <p className="py-4 text-center text-sm text-destructive">
            Failed to load sinks.
          </p>
        ) : sinks.length === 0 ? (
          <p className="py-6 text-center text-sm text-muted-foreground">
            No sinks configured. Add an email sink to start delivering alerts.
          </p>
        ) : (
          <ul className="divide-y divide-border/40">
            {sinks.map((s) => (
              <SinkRow
                key={s.sink_id}
                sink={s}
                result={results[s.sink_id]}
                onResult={(r) =>
                  setResults((prev) => ({ ...prev, [s.sink_id]: r }))
                }
                onEdit={() =>
                  s.config.kind === "email" ? setEditing(s.config) : undefined
                }
                onMutated={invalidate}
              />
            ))}
          </ul>
        )}
      </CardContent>

      {creating || editing ? (
        <EmailSinkSheet
          initial={editing ?? blankEmailSink()}
          isNew={creating}
          onClose={() => {
            setCreating(false);
            setEditing(null);
          }}
          onSaved={() => {
            setCreating(false);
            setEditing(null);
            invalidate();
          }}
        />
      ) : null}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// One row.
// ---------------------------------------------------------------------------

function SinkRow({
  sink,
  result,
  onResult,
  onEdit,
  onMutated,
}: {
  sink: AdminSinkView;
  result?: TestSinkOut;
  onResult: (r: TestSinkOut) => void;
  onEdit: () => void;
  onMutated: () => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  const editable = sink.config.kind === "email" && sink.source === "cloud";

  const test = useMutation({
    mutationFn: () => testSink(sink.kind, sink.name),
    onSuccess: onResult,
    onError: (e: unknown) =>
      onResult({
        sink_id: sink.sink_id,
        ok: false,
        error: e instanceof Error ? e.message : String(e),
      }),
  });

  const del = useMutation({
    mutationFn: () => deleteSink(sink.kind, sink.name),
    onSuccess: () => {
      setConfirmDelete(false);
      onMutated();
    },
  });

  return (
    <li className="flex flex-col gap-2 py-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <Mail className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          <span className="truncate text-sm font-medium">{sink.name}</span>
          <Badge variant="secondary">
            {KIND_LABEL[sink.kind] ?? sink.kind}
          </Badge>
          {sink.active ? (
            <Badge variant="success">active</Badge>
          ) : (
            <Badge variant="warning">inactive</Badge>
          )}
          {sink.source === "file" ? (
            <Badge variant="outline" title="Defined in nexus.toml">
              <FileLock2 className="mr-1 h-3 w-3" />
              nexus.toml
            </Badge>
          ) : null}
        </div>
        <p className="mt-0.5 truncate text-xs text-muted-foreground">
          {sinkSubtitle(sink.config)}
        </p>
        {result ? (
          <p
            className={`mt-1 flex items-start gap-1 text-xs ${
              result.ok ? "text-success" : "text-destructive"
            }`}
          >
            {result.ok ? (
              <CheckCircle2 className="mt-0.5 h-3 w-3 shrink-0" />
            ) : (
              <AlertCircle className="mt-0.5 h-3 w-3 shrink-0" />
            )}
            <span>
              {result.ok
                ? "Test alert delivered."
                : `Test failed${result.transient ? " (transient)" : ""}: ${result.error ?? "unknown error"}`}
            </span>
          </p>
        ) : null}
        {del.isError ? (
          <p className="mt-1 text-xs text-destructive">
            {del.error instanceof Error ? del.error.message : String(del.error)}
          </p>
        ) : null}
      </div>

      <div className="flex shrink-0 items-center gap-1">
        <Button
          variant="ghost"
          size="sm"
          onClick={() => test.mutate()}
          disabled={!sink.active || test.isPending}
          title={
            sink.active
              ? "Send one synthetic alert through this sink"
              : "Sink is not in the live registry"
          }
        >
          {test.isPending ? "Testing…" : "Test"}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          onClick={onEdit}
          disabled={!editable}
          title={
            sink.source === "file"
              ? "Defined in nexus.toml — edit the file on the box"
              : editable
                ? "Edit this sink"
                : "Edit this sink from the cloud console"
          }
        >
          Edit
        </Button>
        {sink.source === "cloud" ? (
          confirmDelete ? (
            <>
              <Button
                variant="destructive"
                size="sm"
                onClick={() => del.mutate()}
                disabled={del.isPending}
              >
                {del.isPending ? "Removing…" : "Confirm"}
              </Button>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => setConfirmDelete(false)}
              >
                Cancel
              </Button>
            </>
          ) : (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setConfirmDelete(true)}
              title="Remove this sink"
            >
              <Trash2 className="h-4 w-4 text-destructive" />
            </Button>
          )
        ) : null}
      </div>
    </li>
  );
}

// ---------------------------------------------------------------------------
// Email sink editor.
// ---------------------------------------------------------------------------

function EmailSinkSheet({
  initial,
  isNew,
  onClose,
  onSaved,
}: {
  initial: EmailSinkConfig;
  isNew: boolean;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [host, setHost] = useState(initial.smtp_host);
  const [port, setPort] = useState(String(initial.smtp_port));
  const [starttls, setStarttls] = useState(initial.starttls);
  const [from, setFrom] = useState(initial.from_address);
  const [fromName, setFromName] = useState(initial.from_name ?? "");
  const [to, setTo] = useState(formatAddressList(initial.to));
  const [cc, setCc] = useState(formatAddressList(initial.cc));
  const [replyTo, setReplyTo] = useState(initial.reply_to ?? "");
  const [subjectPrefix, setSubjectPrefix] = useState(
    initial.subject_prefix ?? "",
  );
  const [attachSnapshot, setAttachSnapshot] = useState(initial.attach_snapshot);
  const [attachClip, setAttachClip] = useState(initial.attach_clip);
  const hadPassword = initial.password === REDACTED_SECRET;
  const [useAuth, setUseAuth] = useState(
    Boolean(initial.username) || hadPassword,
  );
  const [username, setUsername] = useState(initial.username ?? "");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);

  const save = useMutation({
    mutationFn: (cfg: EmailSinkConfig) => putSink("email", cfg.name, cfg),
    onSuccess: onSaved,
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const build = (): EmailSinkConfig | null => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      setError("Name is required.");
      return null;
    }
    if (trimmedName.includes(":")) {
      setError("Name must not contain ':'.");
      return null;
    }
    if (!host.trim()) {
      setError("SMTP host is required.");
      return null;
    }
    const portNum = Number(port);
    if (!Number.isInteger(portNum) || portNum < 1 || portNum > 65535) {
      setError("SMTP port must be between 1 and 65535.");
      return null;
    }
    if (!from.includes("@")) {
      setError("From address must be a valid email address.");
      return null;
    }
    const toList = parseAddressList(to);
    if (toList.length === 0) {
      setError("At least one To recipient is required.");
      return null;
    }
    const ccList = parseAddressList(cc);
    for (const addr of [...toList, ...ccList]) {
      if (!addr.includes("@")) {
        setError(`'${addr}' is not a valid email address.`);
        return null;
      }
    }
    const trimmedReplyTo = replyTo.trim();
    if (trimmedReplyTo && !trimmedReplyTo.includes("@")) {
      setError("Reply-To must be a valid email address.");
      return null;
    }

    let outUser: string | null = null;
    let outPass: string | null = null;
    if (useAuth) {
      const trimmedUser = username.trim();
      if (!trimmedUser) {
        setError("Username is required when the relay needs authentication.");
        return null;
      }
      outUser = trimmedUser;
      if (password) {
        outPass = password;
      } else if (hadPassword) {
        // Echo the sentinel back — the engine restores the stored secret.
        outPass = REDACTED_SECRET;
      } else {
        setError("Password is required when the relay needs authentication.");
        return null;
      }
    }

    return {
      kind: "email",
      name: trimmedName,
      smtp_host: host.trim(),
      smtp_port: portNum,
      starttls,
      from_address: from.trim(),
      from_name: fromName.trim() || null,
      to: toList,
      cc: ccList,
      reply_to: trimmedReplyTo || null,
      subject_prefix: subjectPrefix.trim() || null,
      attach_snapshot: attachSnapshot,
      attach_clip: attachClip,
      username: outUser,
      password: outPass,
      timeout_secs: initial.timeout_secs,
    };
  };

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    const cfg = build();
    if (cfg) save.mutate(cfg);
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title={isNew ? "New email sink" : `Edit ${initial.name}`}
      description="Emails the alert to a recipient list through your own SMTP relay (Microsoft 365, Google Workspace, Exchange, or an on-prem MTA). Credentials stay on this box."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={save.isPending}>
            {save.isPending ? "Saving…" : "Save"}
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
            <Label htmlFor="sink-email-name">Name</Label>
            <Input
              id="sink-email-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={!isNew}
              placeholder="site-ops"
              autoFocus={isNew}
            />
            {!isNew ? (
              <p className="text-xs text-muted-foreground">
                The name is the sink id — create a new sink to change it.
              </p>
            ) : null}
          </div>
        </SheetSection>

        <SheetSection
          title="Recipients"
          description="Comma-separated. Pasted lists with semicolons or newlines are accepted too."
        >
          <div className="space-y-2">
            <Label htmlFor="sink-email-to">To</Label>
            <Input
              id="sink-email-to"
              value={to}
              onChange={(e) => setTo(e.target.value)}
              placeholder="ops@example.com, security@example.com"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="sink-email-cc">Cc (optional)</Label>
            <Input
              id="sink-email-cc"
              value={cc}
              onChange={(e) => setCc(e.target.value)}
              placeholder="records@example.com"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="sink-email-replyto">Reply-To (optional)</Label>
            <Input
              id="sink-email-replyto"
              value={replyTo}
              onChange={(e) => setReplyTo(e.target.value)}
              placeholder="dispatch@example.com"
            />
          </div>
        </SheetSection>

        <SheetSection title="Message">
          <div className="space-y-2">
            <Label htmlFor="sink-email-from">From address</Label>
            <Input
              id="sink-email-from"
              value={from}
              onChange={(e) => setFrom(e.target.value)}
              placeholder="nexus@example.com"
            />
            <p className="text-xs text-muted-foreground">
              Most relays only accept an address they are authorised to send
              as.
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="sink-email-fromname">
              From display name (optional)
            </Label>
            <Input
              id="sink-email-fromname"
              value={fromName}
              onChange={(e) => setFromName(e.target.value)}
              placeholder="Nexus Edge AI"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="sink-email-prefix">Subject prefix (optional)</Label>
            <Input
              id="sink-email-prefix"
              value={subjectPrefix}
              onChange={(e) => setSubjectPrefix(e.target.value)}
              placeholder="[North Yard]"
            />
            <p className="text-xs text-muted-foreground">
              A stable string to build mailbox rules on when one relay serves
              several sites.
            </p>
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={attachSnapshot}
              onChange={(e) => setAttachSnapshot(e.target.checked)}
            />
            Attach the annotated snapshot
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={attachClip}
              onChange={(e) => setAttachClip(e.target.checked)}
            />
            Attach the motion clip
            <span className="text-xs text-muted-foreground">
              (often exceeds relay size limits)
            </span>
          </label>
        </SheetSection>

        <SheetSection title="SMTP relay">
          <div className="grid gap-3 sm:grid-cols-[1fr_8rem]">
            <div className="space-y-2">
              <Label htmlFor="sink-email-host">Host</Label>
              <Input
                id="sink-email-host"
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="smtp.office365.com"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="sink-email-port">Port</Label>
              <Input
                id="sink-email-port"
                value={port}
                onChange={(e) => setPort(e.target.value)}
                inputMode="numeric"
              />
            </div>
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={starttls}
              onChange={(e) => setStarttls(e.target.checked)}
            />
            Negotiate STARTTLS
            <span className="text-xs text-muted-foreground">
              (turn off only for a plaintext relay on a trusted LAN)
            </span>
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="h-4 w-4 rounded border-border"
              checked={useAuth}
              onChange={(e) => setUseAuth(e.target.checked)}
            />
            The relay requires authentication
          </label>
          {useAuth ? (
            <>
              <div className="space-y-2">
                <Label htmlFor="sink-email-username">Username</Label>
                <Input
                  id="sink-email-username"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoComplete="off"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="sink-email-password">Password</Label>
                <Input
                  id="sink-email-password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="new-password"
                  placeholder={
                    hadPassword ? "•••• set — leave blank to keep" : ""
                  }
                />
              </div>
            </>
          ) : null}
        </SheetSection>
      </form>
    </Sheet>
  );
}
