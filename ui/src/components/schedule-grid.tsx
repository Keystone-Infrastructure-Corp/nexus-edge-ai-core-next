// Shared weekly schedule grid (7 days × 48 half-hour slots).
//
// Originally lived inline in pages/delivery.tsx. Extracted so the
// per-rule delivery editor in pages/rules.tsx can reuse the exact
// same drag-to-paint UX without duplication.
//
// Engine wire format (DeliverySchedule.grid): boolean[7][48] where
// row 0 == Mon, slot 0 == 00:00–00:30. `null` schedule in the
// engine means "always on"; this component never renders for that
// case — the caller toggles the schedule on/off and only mounts
// the grid when on.

import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";

const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const HOURS = Array.from({ length: 24 }, (_, h) => h);

// Helper functions are intentionally co-located with the component for
// tight coupling (grid shape is owned by ScheduleGrid). Re-exporting
// breaks `react-refresh/only-export-components` HMR support but the
// page-level Fast Refresh boundary on consumers absorbs it.
// eslint-disable-next-line react-refresh/only-export-components
export function makeEmptyGrid(initial: boolean = true): boolean[][] {
  return Array.from({ length: 7 }, () =>
    Array.from({ length: 48 }, () => initial),
  );
}

// eslint-disable-next-line react-refresh/only-export-components
export function cloneGrid(g: boolean[][]): boolean[][] {
  return g.map((row) => row.slice());
}

// Half-hour label for a slot, e.g. slot 37 -> "18:30". slot 48 -> "24:00".
function slotLabel(slot: number): string {
  const h = Math.floor(slot / 2);
  const m = slot % 2 === 0 ? "00" : "30";
  return `${h.toString().padStart(2, "0")}:${m}`;
}

// Full 30-minute range a slot covers, e.g. slot 37 -> "18:30\u201319:00".
// Shown as a hover tooltip so operators see exactly which block a cell is,
// avoiding the off-by-one-slot mistake (leaving 18:30 suppressed when the
// window was meant to open at 18:30 rather than 19:00).
function slotRangeLabel(slot: number): string {
  return `${slotLabel(slot)}\u2013${slotLabel(slot + 1)}`;
}

export function ScheduleGrid({
  grid,
  onChange,
}: {
  grid: boolean[][];
  onChange: (g: boolean[][]) => void;
}) {
  // Painting mode: while pointer is down, every cell hovered flips to the
  // value set at pointerdown (opposite of the cell that was clicked).
  const [paintValue, setPaintValue] = useState<boolean | null>(null);

  const setCell = (day: number, slot: number, value: boolean) => {
    const row = grid[day];
    if (!row || row[slot] === value) return;
    const next = cloneGrid(grid);
    const nextRow = next[day];
    if (!nextRow) return;
    nextRow[slot] = value;
    onChange(next);
  };

  const onPointerDown =
    (day: number, slot: number) => (e: React.PointerEvent) => {
      e.preventDefault();
      const row = grid[day];
      if (!row) return;
      const next = !row[slot];
      setPaintValue(next);
      setCell(day, slot, next);
    };

  const onPointerEnter = (day: number, slot: number) => () => {
    if (paintValue === null) return;
    setCell(day, slot, paintValue);
  };

  const onPointerUp = () => setPaintValue(null);

  const fillAll = (v: boolean) =>
    onChange(
      Array.from({ length: 7 }, () => Array.from({ length: 48 }, () => v)),
    );

  return (
    <div
      className="space-y-2"
      onPointerUp={onPointerUp}
      onPointerLeave={onPointerUp}
    >
      <div className="flex items-center justify-between">
        <Label className="text-xs text-muted-foreground">
          Drag to paint · click flips
        </Label>
        <div className="flex gap-1">
          <Button
            type="button"
            size="sm"
            variant="ghost"
            onClick={() => fillAll(true)}
          >
            All on
          </Button>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            onClick={() => fillAll(false)}
          >
            All off
          </Button>
        </div>
      </div>

      <div className="schedule-editor overflow-x-auto rounded-md border border-border/40 p-2">
        <table className="select-none border-separate border-spacing-0 text-[10px]">
          <thead>
            <tr>
              <th className="sticky left-0 z-10 w-12 bg-card pr-2 text-left text-muted-foreground"></th>
              {HOURS.map((h) => (
                <th
                  key={h}
                  colSpan={2}
                  className="border-b border-border/30 px-0 pb-1 text-center text-muted-foreground"
                >
                  {h.toString().padStart(2, "0")}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {DAYS.map((day, di) => {
              const row = grid[di] ?? [];
              return (
                <tr key={day}>
                  <td className="sticky left-0 z-10 bg-card pr-2 font-mono text-muted-foreground">
                    {day}
                  </td>
                  {row.map((on, slot) => (
                    <td
                      key={slot}
                      title={`${day} ${slotRangeLabel(slot)} \u00b7 ${
                        on ? "delivery on" : "suppressed"
                      }`}
                      onPointerDown={onPointerDown(di, slot)}
                      onPointerEnter={onPointerEnter(di, slot)}
                      style={{
                        width: 10,
                        height: 18,
                        cursor: "pointer",
                        // "On" cells render in the palette's signature
                        // cyan (--primary, #38e1ff) so they pop against
                        // the dark card surface; the previous --accent
                        // (#1f242b) was nearly invisible. Off cells
                        // stay muted for clear contrast.
                        backgroundColor: on
                          ? "hsl(var(--primary) / 0.85)"
                          : "hsl(var(--muted) / 0.3)",
                        borderRight:
                          slot % 2 === 1
                            ? "1px solid hsl(var(--border) / 0.4)"
                            : undefined,
                        borderBottom: "1px solid hsl(var(--border) / 0.2)",
                      }}
                    />
                  ))}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
