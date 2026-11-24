import { useCallback, useEffect, useState } from "react"
import { listen } from "@tauri-apps/api/event"
import { Badge } from "@whity/ui/badge"
import { Button } from "@whity/ui/button"
import { Card } from "@whity/ui/card"
import { ToggleGroup } from "@whity/ui/toggle-group"
import {
  answerHardwareProposal,
  hardwareProposals,
  hardwareSettings,
  phpHostStatus,
  setHardwareDecision,
  type Decision,
  type HardwareProposal,
  type HardwareProposalEvent,
  type HardwareSetting,
} from "./api"
import { useI18n, type MessageKey } from "./i18n"

/** One plugin's rows, in the order the device reports them. */
export interface PluginGroup {
  plugin: string
  rows: HardwareSetting[]
}

export function groupByPlugin(rows: HardwareSetting[]): PluginGroup[] {
  const groups = new Map<string, HardwareSetting[]>()
  for (const row of rows) {
    groups.set(row.plugin, [...(groups.get(row.plugin) ?? []), row])
  }
  return [...groups.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([plugin, rows]) => ({ plugin, rows }))
}

const KNOWN: Record<string, MessageKey> = {
  "printer.list": "hardware.capability.printer.list",
  "printer.print": "hardware.capability.printer.print",
}

/** The message key that names a capability in plain words, or the fallback. */
export function capabilityKey(capability: string): MessageKey {
  return KNOWN[capability] ?? "hardware.capability.unknown"
}

/** A proposal arrives once when it opens and once when it is answered. */
export function applyProposalEvent(list: HardwareProposal[], event: HardwareProposalEvent): HardwareProposal[] {
  const rest = list.filter((p) => p.id !== event.proposal.id)
  return [...rest, event.proposal]
}

/** The queue (oldest first) and what was answered (newest first). */
export function splitProposals(list: HardwareProposal[]): { waiting: HardwareProposal[]; recent: HardwareProposal[] } {
  const waiting = list.filter((p) => p.outcome.state === "waiting").sort((a, b) => a.createdAt - b.createdAt)
  const recent = list.filter((p) => p.outcome.state !== "waiting").sort((a, b) => b.createdAt - a.createdAt)
  return { waiting, recent }
}

const DECISIONS: Decision[] = ["off", "ask_first", "allowed"]

/**
 * What each plugin may do with this computer's hardware, and the requests a
 * plugin set to Ask first is waiting on (ADR 0001, 3a). One request at a
 * time, oldest first, as the approvals inbox does; a waiting request wears
 * the dashed edge because nothing has happened yet. No design card exists for
 * this app yet (WHIT-821); built from tokens and the shared components.
 */
export function HardwarePanel() {
  const { t } = useI18n()
  const [running, setRunning] = useState<boolean | null>(null)
  const [settings, setSettings] = useState<HardwareSetting[]>([])
  const [proposals, setProposals] = useState<HardwareProposal[]>([])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(() => {
    phpHostStatus().then((status) => setRunning(status.state === "ready"), () => setRunning(false))
    hardwareSettings().then(setSettings, (e: unknown) => setError(String(e)))
    hardwareProposals().then(setProposals, (e: unknown) => setError(String(e)))
  }, [])

  useEffect(refresh, [refresh])

  // The host hands out handles when it becomes ready; the rows follow.
  useEffect(() => {
    const stop = listen("php:status", refresh)
    return () => void stop.then((unlisten) => unlisten(), () => undefined)
  }, [refresh])

  useEffect(() => {
    const stop = listen<HardwareProposalEvent>("hardware:proposal", (event) =>
      setProposals((current) => applyProposalEvent(current, event.payload)),
    )
    return () => void stop.then((unlisten) => unlisten(), () => undefined)
  }, [])

  const decide = async (row: HardwareSetting, decision: Decision) => {
    setError(null)
    try {
      setSettings(await setHardwareDecision(row.plugin, row.capability, decision))
    } catch (e) {
      setError(String(e))
    }
  }

  const answer = async (id: string, approve: boolean) => {
    setBusy(true)
    setError(null)
    try {
      const answered = await answerHardwareProposal(id, approve)
      setProposals((current) => applyProposalEvent(current, { kind: "answered", proposal: answered }))
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const action = (capability: string) => t(capabilityKey(capability), { capability })
  const { waiting, recent } = splitProposals(proposals)
  const current = waiting[0]
  const groups = groupByPlugin(settings)

  return (
    <>
      <section className="flex flex-col gap-3">
        <h2 className="text-base font-semibold">{t("hardware.proposals")}</h2>
        {current === undefined ? (
          <p className="text-muted-foreground">{t("hardware.noProposals")}</p>
        ) : (
          <Card borderStyle="dashed" className="flex flex-col gap-3 p-4">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <Badge variant="outline">
                <span aria-hidden="true" className="size-2 rounded-full border-[1.5px] border-dashed border-brand" />
                {t("hardware.waiting")}
              </Badge>
              <span className="text-xs text-muted-foreground">
                {t("hardware.position", { index: "1", count: String(waiting.length) })}
              </span>
            </div>
            <p className="font-medium">{t("hardware.asks", { plugin: current.plugin, action: action(current.capability) })}</p>
            <ProposalDetails proposal={current} />
            <div className="flex gap-2">
              <Button disabled={busy} onClick={() => answer(current.id, true)}>
                {t("hardware.approve")}
              </Button>
              <Button variant="outline" disabled={busy} onClick={() => answer(current.id, false)}>
                {t("hardware.decline")}
              </Button>
            </div>
          </Card>
        )}
        {recent.length > 0 && (
          <ul className="flex flex-col gap-1">
            {recent.slice(0, 5).map((p) => (
              <li key={p.id} className="flex items-center justify-between gap-3 text-xs text-muted-foreground">
                <span className="truncate">{t("hardware.asks", { plugin: p.plugin, action: action(p.capability) })}</span>
                <OutcomeBadge proposal={p} />
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="flex flex-col gap-3">
        <h2 className="text-base font-semibold">{t("hardware.title")}</h2>
        <p className="text-muted-foreground">{t("hardware.intro")}</p>
        {running === false && <p className="text-muted-foreground">{t("hardware.notRunning")}</p>}
        {running !== false && groups.length === 0 && <p className="text-muted-foreground">{t("hardware.none")}</p>}
        {groups.map((group) => (
          <div key={group.plugin} className="flex flex-col gap-2 rounded-card border border-border bg-card p-4">
            <h3 className="font-medium">{group.plugin}</h3>
            <ul className="flex flex-col gap-2">
              {group.rows.map((row) => (
                <li key={row.capability} className="flex flex-wrap items-center justify-between gap-3">
                  <span className="flex min-w-0 flex-col">
                    <span>{action(row.capability)}</span>
                    <span className="text-xs text-muted-foreground">
                      <span dir="ltr">{row.capability}</span>
                      {row.isDefault && <> · {t("hardware.default")}</>}
                    </span>
                  </span>
                  <ToggleGroup
                    type="single"
                    required
                    size="sm"
                    label={`${group.plugin} · ${action(row.capability)}`}
                    value={row.decision}
                    onChange={(value) => {
                      if (typeof value === "string" && DECISIONS.includes(value as Decision) && value !== row.decision) {
                        decide(row, value as Decision)
                      }
                    }}
                    options={[
                      { value: "off", label: t("hardware.off") },
                      { value: "ask_first", label: t("hardware.askFirst") },
                      { value: "allowed", label: t("hardware.allowed") },
                    ]}
                  />
                </li>
              ))}
            </ul>
          </div>
        ))}
        {error !== null && (
          <p role="alert" className="text-error-text">
            {error}
          </p>
        )}
      </section>
    </>
  )
}

/** The arguments the plugin sent, for the person to see before approving. */
function ProposalDetails({ proposal }: { proposal: HardwareProposal }) {
  const { t } = useI18n()
  if (proposal.args === null || proposal.args === undefined) return null
  return (
    <details className="text-xs text-muted-foreground">
      <summary className="cursor-pointer">{t("hardware.details")}</summary>
      <pre dir="ltr" className="mt-1 overflow-x-auto rounded-control bg-muted p-2 font-mono text-start">
        {JSON.stringify(proposal.args, null, 2)}
      </pre>
    </details>
  )
}

function OutcomeBadge({ proposal }: { proposal: HardwareProposal }) {
  const { t } = useI18n()
  switch (proposal.outcome.state) {
    case "done":
      return <Badge variant="success">{t("hardware.done")}</Badge>
    case "failed":
      return <Badge variant="warning">{t("hardware.failed", { reason: proposal.outcome.error.message })}</Badge>
    case "declined":
      return <Badge variant="outline">{t("hardware.declined")}</Badge>
    case "expired":
      return <Badge variant="outline">{t("hardware.expired")}</Badge>
    default:
      return null
  }
}
