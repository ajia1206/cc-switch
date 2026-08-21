import { describe, expect, it } from "vitest";
import {
  createCCIslandLayout,
  fitIslandSize,
  getTopCenterPosition,
  type AvailableScreenBounds,
} from "./ccIslandLayout";

const primary: AvailableScreenBounds = {
  left: 0,
  top: 25,
  width: 1440,
  height: 875,
};

describe("CC Island layout", () => {
  it("centers the compact island below the macOS reserved top area", () => {
    expect(createCCIslandLayout("compact", primary)).toEqual({
      size: { width: 360, height: 64 },
      position: { x: 540, y: 33 },
    });
  });

  it("preserves negative coordinates for a display left of the primary screen", () => {
    const leftDisplay: AvailableScreenBounds = {
      left: -1920,
      top: 0,
      width: 1920,
      height: 1080,
    };

    expect(
      getTopCenterPosition(leftDisplay, { width: 420, height: 220 }),
    ).toEqual({ x: -1170, y: 8 });
  });

  it("fits the details panel inside a small usable work area", () => {
    const small: AvailableScreenBounds = {
      left: 0,
      top: 24,
      width: 320,
      height: 420,
    };

    expect(fitIslandSize("details", small)).toEqual({
      width: 304,
      height: 404,
    });
    expect(createCCIslandLayout("details", small).position).toEqual({
      x: 8,
      y: 32,
    });
  });

  it("clamps an off-spec oversized panel without producing invalid positions", () => {
    const bounds: AvailableScreenBounds = {
      left: 100,
      top: 40,
      width: 250,
      height: 100,
    };

    expect(
      getTopCenterPosition(bounds, { width: 400, height: 200 }),
    ).toEqual({ x: 108, y: 48 });
  });
});
