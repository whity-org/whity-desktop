import { describe, expect, it } from "vitest"
import { hoursLeft, reasonKey } from "./device-row"

describe("hoursLeft", () => {
  it("rounds the remaining offline window down to whole hours", () => {
    expect(hoursLeft(3 * 3600 + 1799)).toBe("3")
    expect(hoursLeft(59)).toBe("0")
    expect(hoursLeft(null)).toBe("0")
    expect(hoursLeft(-5)).toBe("0")
  })
})

describe("reasonKey", () => {
  it("names the reasons the app has words for and leaves the rest raw", () => {
    expect(reasonKey("not_verified:no_keys")).toBe("plugins.reason.not_verified:no_keys")
    expect(reasonKey("checksum")).toBe("plugins.reason.checksum")
    expect(reasonKey("something_else")).toBeNull()
  })
})
