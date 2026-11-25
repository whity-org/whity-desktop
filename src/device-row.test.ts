import { describe, expect, it } from "vitest"
import { hoursLeft } from "./device-row"

describe("hoursLeft", () => {
  it("rounds the remaining offline window down to whole hours", () => {
    expect(hoursLeft(3 * 3600 + 1799)).toBe("3")
    expect(hoursLeft(59)).toBe("0")
    expect(hoursLeft(null)).toBe("0")
    expect(hoursLeft(-5)).toBe("0")
  })
})
