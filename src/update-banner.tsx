import { useEffect, useState } from "react"
import { Button } from "@whity/ui/button"
import { ToggleGroup } from "@whity/ui/toggle-group"
import { checkForUpdate, getUpdateChannel, installUpdate, setUpdateChannel, type UpdateChannel, type UpdateFound } from "./api"
import { useI18n, type MessageKey } from "./i18n"

const DAY = 24 * 60 * 60 * 1000
const CHANNELS: UpdateChannel[] = ["stable", "beta"]

/** Where an update came from, in the person's words. */
export function sourceKey(found: Pick<UpdateFound, "source">): MessageKey {
  return found.source === "workspace" ? "update.fromWorkspace" : "update.fromWhity"
}

/**
 * Updates download quietly on start and once a day, and wait here for the
 * person to choose when to restart. A failed check says nothing: the app works
 * as it is, and the next check tries again. The workspace's core is asked
 * first, so an operator decides what their devices run; Whity's own feed
 * answers when no workspace offers anything (ADR 0001, decision 2b).
 */
export function UpdateBanner() {
  const { t } = useI18n()
  const [found, setFound] = useState<UpdateFound | null>(null)
  const [busy, setBusy] = useState(false)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    const check = () => {
      checkForUpdate().then(
        (ready) => setFound((current) => ready ?? current),
        () => undefined,
      )
    }
    check()
    const timer = setInterval(check, DAY)
    return () => clearInterval(timer)
  }, [])

  if (found === null) return null

  const restart = async () => {
    setBusy(true)
    setFailed(false)
    try {
      await installUpdate()
    } catch {
      setFailed(true)
      setBusy(false)
    }
  }

  return (
    <div role="status" className="flex items-center justify-between gap-3 rounded-card border border-border bg-card p-4">
      <span>
        {t("update.ready")} <span dir="ltr">{found.version}</span>
        <span className="block text-xs text-muted-foreground">{t(sourceKey(found))}</span>
        {failed && <span className="block text-error-text">{t("update.failed")}</span>}
      </span>
      <Button variant="outline" disabled={busy} onClick={restart}>
        {t("update.restart")}
      </Button>
    </div>
  )
}

/**
 * Stable releases only, or beta ones too (ADR 0001, decision 6): a choice per
 * computer, kept on the native side and carried to whichever feed answers.
 */
export function UpdateChannelChoice() {
  const { t } = useI18n()
  const [channel, setChannel] = useState<UpdateChannel | null>(null)

  useEffect(() => {
    getUpdateChannel().then(setChannel, () => setChannel("stable"))
  }, [])

  if (channel === null) return null

  const choose = async (value: UpdateChannel) => {
    setChannel(value)
    try {
      await setUpdateChannel(value)
    } catch {
      // The old choice stands on the native side; the next open shows it.
    }
  }

  return (
    <div className="flex flex-wrap items-center justify-between gap-3 text-sm">
      <span className="flex flex-col">
        <span>{t("update.channel")}</span>
        <span className="text-xs text-muted-foreground">{t("update.channelNote")}</span>
      </span>
      <ToggleGroup
        type="single"
        required
        size="sm"
        label={t("update.channel")}
        value={channel}
        onChange={(value) => {
          if (typeof value === "string" && CHANNELS.includes(value as UpdateChannel) && value !== channel) {
            void choose(value as UpdateChannel)
          }
        }}
        options={CHANNELS.map((c) => ({ value: c, label: t(c === "stable" ? "update.stable" : "update.beta") }))}
      />
    </div>
  )
}
