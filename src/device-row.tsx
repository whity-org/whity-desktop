import { useEffect, useState } from "react"
import { Button } from "@whity/ui/button"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@whity/ui/select"
import {
  chooseDeviceWorkspace,
  enrolDevice,
  forgetDevice,
  deviceStatus,
  listSignInProviders,
  type Connection,
  type Device,
  type Membership,
  type SignInProvider,
} from "./api"
import { useI18n } from "./i18n"

/** Words for the hours an enrolment still works offline. */
export function hoursLeft(secondsRemaining: number | null): string {
  if (secondsRemaining === null) return "0"
  return String(Math.max(0, Math.floor(secondsRemaining / 3600)))
}

/**
 * This computer as a device of the workspace (ADR 0001, decision 5): connect
 * it through the workspace's identity provider in the browser, see that it is
 * connected and for how long it works offline, and disconnect it. The
 * credential never comes here; the row shows what Rust reports.
 */
export function DeviceRow({ connection }: { connection: Connection }) {
  const { t } = useI18n()
  const [device, setDevice] = useState<Device | null>(null)
  const [providers, setProviders] = useState<SignInProvider[] | null>(null)
  const [provider, setProvider] = useState<string>("")
  const [memberships, setMemberships] = useState<Membership[] | null>(null)
  const [tenant, setTenant] = useState<string>("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    deviceStatus(connection.id).then(setDevice, () => setDevice(null))
  }, [connection.id])

  const describe = (e: unknown) => {
    const code = String(e)
    if (code === "keychain") return t("device.keychain")
    if (code === "device_revoked") return t("device.revoked")
    return t("device.failed", { reason: code })
  }

  const prepare = async () => {
    setError(null)
    try {
      const found = await listSignInProviders(connection.id)
      setProviders(found)
      if (found.length === 1) await connect(found[0].provider_key)
      else if (found.length > 1) setProvider(found[0].provider_key)
    } catch (e) {
      setError(describe(e))
    }
  }

  const connect = async (key: string) => {
    setBusy(true)
    setError(null)
    try {
      const outcome = await enrolDevice(connection.id, key)
      if (outcome.outcome === "enrolled") {
        setDevice(outcome.device)
        setProviders(null)
      } else {
        setMemberships(outcome.memberships)
        setTenant(String(outcome.memberships[0]?.tenantId ?? ""))
      }
    } catch (e) {
      setError(describe(e))
    } finally {
      setBusy(false)
    }
  }

  const choose = async () => {
    setBusy(true)
    setError(null)
    try {
      setDevice(await chooseDeviceWorkspace(connection.id, Number(tenant)))
      setMemberships(null)
      setProviders(null)
    } catch (e) {
      setError(describe(e))
    } finally {
      setBusy(false)
    }
  }

  const disconnect = async () => {
    setBusy(true)
    try {
      await forgetDevice(connection.id)
      setDevice(null)
    } catch (e) {
      setError(describe(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex basis-full flex-wrap items-center justify-between gap-3 border-t border-border pt-3">
      {device !== null ? (
        <>
          <span className="flex min-w-0 flex-col text-xs text-muted-foreground">
            <span>{t("device.connected", { name: device.name })}</span>
            <span>{device.locked ? t("device.locked") : t("device.remaining", { hours: hoursLeft(device.secondsRemaining) })}</span>
          </span>
          <Button variant="outline" disabled={busy} onClick={disconnect}>
            {t("device.disconnect")}
          </Button>
        </>
      ) : memberships !== null ? (
        <>
          <span className="text-xs text-muted-foreground">{t("device.chooseWorkspace")}</span>
          <div className="flex items-center gap-2">
            <Select value={tenant} onValueChange={setTenant}>
              <SelectTrigger aria-label={t("device.chooseWorkspace")}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {memberships.map((m) => (
                  <SelectItem key={m.tenantId} value={String(m.tenantId)}>
                    {m.name ?? String(m.tenantId)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant="outline" disabled={busy || tenant === ""} onClick={choose}>
              {t("device.useWorkspace")}
            </Button>
          </div>
        </>
      ) : providers !== null && providers.length > 1 ? (
        <>
          <span className="text-xs text-muted-foreground">{t("device.provider")}</span>
          <div className="flex items-center gap-2">
            <Select value={provider} onValueChange={setProvider}>
              <SelectTrigger aria-label={t("device.provider")}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {providers.map((p) => (
                  <SelectItem key={p.provider_key} value={p.provider_key}>
                    {p.display_name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant="outline" disabled={busy} onClick={() => connect(provider)}>
              {busy ? t("device.connecting") : t("device.connect")}
            </Button>
          </div>
        </>
      ) : (
        <>
          <span className="text-xs text-muted-foreground">
            {providers !== null && providers.length === 0 ? t("device.noProvider") : busy ? t("device.connecting") : ""}
          </span>
          <Button variant="outline" disabled={busy || (providers !== null && providers.length === 0)} onClick={prepare}>
            {t("device.connect")}
          </Button>
        </>
      )}
      {error !== null && (
        <p role="alert" className="basis-full text-xs text-error-text">
          {error}
        </p>
      )}
    </div>
  )
}
