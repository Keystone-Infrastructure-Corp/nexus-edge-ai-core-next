// Visual prompts gallery + upload.
//
// Engine returns metadata-only (no preview thumbnails) so v1 is a card
// grid with name + description + delete. Attach-to-camera lives on the
// camera edit sheet, not here.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Image as ImageIcon, Plus, Trash2, Upload } from "lucide-react";
import { useState } from "react";

import {
  deleteVisualPrompt,
  listVisualPrompts,
  uploadVisualPrompt,
} from "@/api/config";
import type { VisualPromptSummary } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { FleetManagedBadge } from "@/components/fleet-managed-badge";

export function VisualPromptsPage() {
  const qc = useQueryClient();
  const promptsQuery = useQuery({
    queryKey: ["visual-prompts", "list"],
    queryFn: listVisualPrompts,
    staleTime: 30_000,
  });

  const [uploadOpen, setUploadOpen] = useState(false);

  const prompts = promptsQuery.data ?? [];

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Visual prompts</h1>
          <p className="text-sm text-muted-foreground">
            Reference images for open-vocab detection. Attach to cameras
            from the Cameras page.
          </p>
        </div>
        <div className="flex items-center gap-3">
          <FleetManagedBadge category="visual_prompts" />
          <Button onClick={() => setUploadOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            Upload
          </Button>
        </div>
      </header>

      {promptsQuery.isLoading ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-32 w-full" />
          ))}
        </div>
      ) : promptsQuery.isError ? (
        <Card>
          <CardContent className="py-8 text-center text-sm text-destructive">
            Failed to load visual prompts. Encoder may be unconfigured
            (engine returns 503 when{" "}
            <code className="font-mono">inference.model.pack_path</code>{" "}
            is unset).
          </CardContent>
        </Card>
      ) : prompts.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
            <ImageIcon className="h-8 w-8 opacity-50" />
            <p>No visual prompts yet.</p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => setUploadOpen(true)}
            >
              Upload your first
            </Button>
          </CardContent>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {prompts.map((p) => (
            <PromptCard
              key={p.id}
              prompt={p}
              onDeleted={() =>
                qc.invalidateQueries({ queryKey: ["visual-prompts", "list"] })
              }
            />
          ))}
        </div>
      )}

      {uploadOpen ? (
        <UploadSheet
          onClose={() => setUploadOpen(false)}
          onSaved={() => {
            setUploadOpen(false);
            qc.invalidateQueries({ queryKey: ["visual-prompts", "list"] });
          }}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Card.
// ---------------------------------------------------------------------------

function PromptCard({
  prompt,
  onDeleted,
}: {
  prompt: VisualPromptSummary;
  onDeleted: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const delMutation = useMutation({
    mutationFn: (id: string) => deleteVisualPrompt(id),
    onSuccess: onDeleted,
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  return (
    <Card>
      <CardContent className="space-y-3 p-4">
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0">
            <p className="truncate font-medium">{prompt.name}</p>
            <p className="font-mono text-xs text-muted-foreground">
              {prompt.id}
            </p>
          </div>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              if (confirm(`Delete visual prompt "${prompt.name}"?`)) {
                delMutation.mutate(prompt.id);
              }
            }}
            disabled={delMutation.isPending}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
        {prompt.description ? (
          <p className="text-xs text-muted-foreground">{prompt.description}</p>
        ) : null}
        {prompt.label ? (
          <p className="text-xs">
            <span className="text-muted-foreground">Label: </span>
            <span className="font-mono">{prompt.label}</span>
          </p>
        ) : null}
        {error ? (
          <p className="text-xs text-destructive">{error}</p>
        ) : null}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Upload sheet.
// ---------------------------------------------------------------------------

function UploadSheet({
  onClose,
  onSaved,
}: {
  onClose: () => void;
  onSaved: () => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [error, setError] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: () => {
      if (!file) throw new Error("Pick an image first.");
      return uploadVisualPrompt({
        name: name.trim(),
        description: description.trim() || undefined,
        image: file,
      });
    },
    onSuccess: onSaved,
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim() || !file) {
      setError("Name and image are required.");
      return;
    }
    mutation.mutate();
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title="Upload visual prompt"
      description="PNG or JPEG. The engine generates an embedding and stores the image."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={mutation.isPending}>
            <Upload className="mr-2 h-4 w-4" />
            {mutation.isPending ? "Uploading…" : "Upload"}
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
            <Label htmlFor="vp-name">Name</Label>
            <Input
              id="vp-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Forklift (yellow)"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="vp-desc">Description (optional)</Label>
            <Input
              id="vp-desc"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="Yellow Toyota 8FBCU25 forklift"
            />
          </div>
        </SheetSection>
        <SheetSection title="Image">
          <div className="space-y-2">
            <Label htmlFor="vp-file">PNG / JPEG</Label>
            <Input
              id="vp-file"
              type="file"
              accept="image/png,image/jpeg"
              onChange={(e) => setFile(e.target.files?.[0] ?? null)}
            />
            {file ? (
              <p className="text-xs text-muted-foreground">
                {file.name} · {(file.size / 1024).toFixed(1)} KB
              </p>
            ) : null}
          </div>
        </SheetSection>
      </form>
    </Sheet>
  );
}
