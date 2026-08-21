export type CCIslandMode = "compact" | "expanded" | "details";

export interface CCIslandSize {
  width: number;
  height: number;
}

export interface CCIslandPoint {
  x: number;
  y: number;
}

export interface AvailableScreenBounds {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface ScreenMetricsLike {
  availLeft?: number;
  availTop?: number;
  availWidth?: number;
  availHeight?: number;
  width?: number;
  height?: number;
}

export interface CCIslandLayout {
  size: CCIslandSize;
  position: CCIslandPoint;
}

export const CC_ISLAND_MARGIN = 8;

export const CC_ISLAND_SIZES: Record<CCIslandMode, CCIslandSize> = {
  compact: { width: 360, height: 64 },
  expanded: { width: 420, height: 220 },
  details: { width: 380, height: 560 },
};

function finiteOr(value: number | undefined, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : fallback;
}

function positiveOr(value: number | undefined, fallback: number): number {
  const candidate = finiteOr(value, fallback);
  return candidate > 0 ? candidate : fallback;
}

/**
 * Normalizes browser screen metrics so layout still works in older WebViews
 * that do not expose availLeft/availTop or briefly report a zero-sized screen.
 */
export function getAvailableScreenBounds(
  screenLike: ScreenMetricsLike,
): AvailableScreenBounds {
  return {
    left: finiteOr(screenLike.availLeft, 0),
    top: finiteOr(screenLike.availTop, 0),
    width: positiveOr(
      screenLike.availWidth,
      positiveOr(screenLike.width, 1440),
    ),
    height: positiveOr(
      screenLike.availHeight,
      positiveOr(screenLike.height, 900),
    ),
  };
}

export function fitIslandSize(
  mode: CCIslandMode,
  bounds: AvailableScreenBounds,
  margin: number = CC_ISLAND_MARGIN,
): CCIslandSize {
  const desired = CC_ISLAND_SIZES[mode];
  const safeMargin = Math.max(0, margin);
  const maxWidth = Math.max(1, bounds.width - safeMargin * 2);
  const maxHeight = Math.max(1, bounds.height - safeMargin * 2);

  return {
    width: Math.round(Math.min(desired.width, maxWidth)),
    height: Math.round(Math.min(desired.height, maxHeight)),
  };
}

/**
 * Places the island at the horizontal center of the current display's usable
 * area and below the system-reserved top area. Negative display coordinates
 * are preserved, which is required when an external monitor sits left of the
 * primary display.
 */
export function getTopCenterPosition(
  bounds: AvailableScreenBounds,
  size: CCIslandSize,
  margin: number = CC_ISLAND_MARGIN,
): CCIslandPoint {
  const safeMargin = Math.max(0, margin);
  const minX = bounds.left + safeMargin;
  const maxX = Math.max(
    minX,
    bounds.left + bounds.width - size.width - safeMargin,
  );
  const centeredX = bounds.left + (bounds.width - size.width) / 2;
  const x = Math.min(maxX, Math.max(minX, centeredX));

  const minY = bounds.top + safeMargin;
  const maxY = Math.max(
    minY,
    bounds.top + bounds.height - size.height - safeMargin,
  );

  return {
    x: Math.round(x),
    y: Math.round(Math.min(maxY, minY)),
  };
}

export function createCCIslandLayout(
  mode: CCIslandMode,
  bounds: AvailableScreenBounds,
  margin: number = CC_ISLAND_MARGIN,
): CCIslandLayout {
  const size = fitIslandSize(mode, bounds, margin);
  return {
    size,
    position: getTopCenterPosition(bounds, size, margin),
  };
}
