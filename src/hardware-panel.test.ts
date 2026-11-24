import { describe, expect, it } from "vitest"
import { applyProposalEvent, capabilityKey, groupByPlugin, splitProposals } from "./hardware-panel"
import type { HardwareProposal, HardwareSetting } from "./api"

const proposal = (id: string, state: HardwareProposal["outcome"]["state"], createdAt: number): HardwareProposal => ({
  id,
  plugin: "BridgeProbe",
  capability: "printer.print",
  args: { text: "x" },
  createdAt,
  outcome: state === "done" ? { state, result: {} } : state === "failed" ? { state, error: { code: "failed", message: "no paper" } } : { state },
})

describe("groupByPlugin", () => {
  it("keeps one group per plugin, in order", () => {
    const rows: HardwareSetting[] = [
      { plugin: "B", capability: "printer.print", decision: "allowed", isDefault: true },
      { plugin: "A", capability: "printer.list", decision: "off", isDefault: false },
      { plugin: "B", capability: "printer.list", decision: "ask_first", isDefault: false },
    ]
    expect(groupByPlugin(rows).map((g) => [g.plugin, g.rows.map((r) => r.capability)])).toEqual([
      ["A", ["printer.list"]],
      ["B", ["printer.print", "printer.list"]],
    ])
  })
})

describe("capabilityKey", () => {
  it("names the capabilities this app has words for, and falls back otherwise", () => {
    expect(capabilityKey("printer.print")).toBe("hardware.capability.printer.print")
    expect(capabilityKey("printer.list")).toBe("hardware.capability.printer.list")
    expect(capabilityKey("scanner.scan")).toBe("hardware.capability.unknown")
  })
})

describe("applyProposalEvent", () => {
  it("adds an opened proposal once and replaces an answered one", () => {
    const a = proposal("a", "waiting", 1)
    let list = applyProposalEvent([], { kind: "opened", proposal: a })
    list = applyProposalEvent(list, { kind: "opened", proposal: a })
    expect(list).toEqual([a])
    const done = proposal("a", "done", 1)
    list = applyProposalEvent(list, { kind: "answered", proposal: done })
    expect(list).toEqual([done])
  })
})

describe("splitProposals", () => {
  it("puts the oldest waiting one first and the answered ones newest first", () => {
    const { waiting, recent } = splitProposals([
      proposal("new", "waiting", 30),
      proposal("old", "waiting", 10),
      proposal("d1", "declined", 20),
      proposal("d2", "done", 25),
    ])
    expect(waiting.map((p) => p.id)).toEqual(["old", "new"])
    expect(recent.map((p) => p.id)).toEqual(["d2", "d1"])
  })
})
