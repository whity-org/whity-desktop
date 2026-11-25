import { useState } from "react"
import { Button } from "@whity/ui/button"
import { Input } from "@whity/ui/input"
import { RadioGroup, RadioGroupItem } from "@whity/ui/radio-group"
import type { SyncChoice, SyncConflict, SyncConflictField } from "./api"
import { useI18n } from "./i18n"

/** Where a field's final value comes from. */
export type Pick = { from: "here" } | { from: "there" } | { from: "own"; value: string }

/** A value as the card shows it: text as is, nothing as a dash, the rest as JSON. */
export function showValue(value: unknown): string {
  if (value === null || value === undefined) return "—"
  if (typeof value === "string") return value
  return JSON.stringify(value)
}

/** The row that results from the picks, one value per differing field. */
export function mergedFields(fields: SyncConflictField[], picks: Record<string, Pick>): Record<string, unknown> {
  const merged: Record<string, unknown> = {}
  for (const field of fields) {
    const pick = picks[field.name] ?? { from: "there" }
    merged[field.name] = pick.from === "here" ? field.local : pick.from === "there" ? field.remote : pick.value
  }
  return merged
}

/** A pick is complete when every own value has been written. */
export function picksComplete(fields: SyncConflictField[], picks: Record<string, Pick>): boolean {
  return fields.every((field) => {
    const pick = picks[field.name]
    return pick === undefined || pick.from !== "own" || pick.value.trim() !== ""
  })
}

/**
 * One parked row (ADR 0001, decision 4): both this computer and the workspace
 * changed it, so a person decides. Both ends live: each differing field takes
 * this computer's value, the workspace's, or one written here, and the merged
 * row goes to both ends. One end deleted it: delete in both places, or keep
 * the changed row as a new record. The values are the person's own content in
 * whatever language, so each one sets its own direction.
 */
export function ConflictCard({
  conflict,
  busy,
  onResolve,
}: {
  conflict: SyncConflict
  busy: boolean
  onResolve: (choice: SyncChoice) => void
}) {
  const { t } = useI18n()
  const [picks, setPicks] = useState<Record<string, Pick>>({})
  const deleted = conflict.localDeleted || conflict.remoteDeleted
  const pickOf = (name: string): Pick => picks[name] ?? { from: "there" }

  return (
    <div className="flex flex-col gap-3 rounded-md border border-border bg-card p-3 text-card-foreground">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <span dir="auto" className="text-sm">
          {conflict.title ?? t("conflicts.untitled")}
        </span>
        <span className="font-mono text-xs text-muted-foreground" dir="ltr">
          {conflict.resource}
        </span>
      </div>
      {deleted ? (
        <>
          <span className="text-xs text-muted-foreground">
            {conflict.localDeleted ? t("conflicts.deletedHere") : t("conflicts.deletedThere")}
          </span>
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" disabled={busy} onClick={() => onResolve({ kind: "delete_both" })}>
              {t("conflicts.deleteBoth")}
            </Button>
            <Button variant="outline" disabled={busy} onClick={() => onResolve({ kind: "keep_as_new" })}>
              {t("conflicts.keepAsNew")}
            </Button>
          </div>
        </>
      ) : (
        <>
          {conflict.fields.map((field) => {
            const pick = pickOf(field.name)
            return (
              <div key={field.name} className="flex flex-col gap-1.5">
                <span className="font-mono text-xs text-muted-foreground" dir="ltr">
                  {field.name}
                </span>
                <RadioGroup
                  value={pick.from}
                  onValueChange={(from) =>
                    setPicks((prev) => {
                      const previous = prev[field.name]
                      const own = previous !== undefined && previous.from === "own" ? previous.value : ""
                      return { ...prev, [field.name]: from === "own" ? { from: "own", value: own } : { from: from as "here" | "there" } }
                    })
                  }
                >
                  <label className="flex items-center gap-2 text-sm">
                    <RadioGroupItem value="here" aria-label={`${field.name} ${t("conflicts.here")}`} />
                    <span>{t("conflicts.here")}:</span>
                    <span dir="auto" className="text-muted-foreground">
                      {showValue(field.local)}
                    </span>
                  </label>
                  <label className="flex items-center gap-2 text-sm">
                    <RadioGroupItem value="there" aria-label={`${field.name} ${t("conflicts.there")}`} />
                    <span>{t("conflicts.there")}:</span>
                    <span dir="auto" className="text-muted-foreground">
                      {showValue(field.remote)}
                    </span>
                  </label>
                  <label className="flex items-center gap-2 text-sm">
                    <RadioGroupItem value="own" aria-label={`${field.name} ${t("conflicts.own")}`} />
                    <span>{t("conflicts.own")}</span>
                  </label>
                </RadioGroup>
                {pick.from === "own" && (
                  <Input
                    dir="auto"
                    aria-label={`${field.name} ${t("conflicts.own")}`}
                    value={pick.value}
                    onChange={(e) => setPicks((prev) => ({ ...prev, [field.name]: { from: "own", value: e.target.value } }))}
                  />
                )}
              </div>
            )
          })}
          <div className="flex justify-end">
            <Button
              variant="outline"
              disabled={busy || !picksComplete(conflict.fields, picks)}
              onClick={() => onResolve({ kind: "fields", fields: mergedFields(conflict.fields, picks) })}
            >
              {t("conflicts.resolve")}
            </Button>
          </div>
        </>
      )}
    </div>
  )
}
