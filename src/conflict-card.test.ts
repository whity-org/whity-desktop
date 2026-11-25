import { describe, expect, it } from "vitest"
import { mergedFields, picksComplete, showValue } from "./conflict-card"

const fields = [
  { name: "name", local: "Mine", remote: "Theirs" },
  { name: "status", local: "active", remote: "archived" },
]

describe("mergedFields", () => {
  it("takes the workspace's value unless told otherwise", () => {
    expect(mergedFields(fields, {})).toEqual({ name: "Theirs", status: "archived" })
    expect(mergedFields(fields, { name: { from: "here" } })).toEqual({ name: "Mine", status: "archived" })
    expect(mergedFields(fields, { name: { from: "own", value: "Agreed" } })).toEqual({ name: "Agreed", status: "archived" })
  })
})

describe("picksComplete", () => {
  it("waits for an own value to be written", () => {
    expect(picksComplete(fields, {})).toBe(true)
    expect(picksComplete(fields, { name: { from: "own", value: "  " } })).toBe(false)
    expect(picksComplete(fields, { name: { from: "own", value: "Agreed" } })).toBe(true)
  })
})

describe("showValue", () => {
  it("shows text as is, nothing as a dash and the rest as JSON", () => {
    expect(showValue("Bread")).toBe("Bread")
    expect(showValue(null)).toBe("—")
    expect(showValue(3)).toBe("3")
    expect(showValue({ a: 1 })).toBe('{"a":1}')
  })
})
