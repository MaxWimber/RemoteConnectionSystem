// Pure helpers for mouse input: where a pointer is over the picture, wheel units and button numbers.

import type { MouseButton } from './protocol';

export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The part of a `<video>` with `object-fit: contain` that shows the picture. */
export function contentBox(element: Box, videoWidth: number, videoHeight: number): Box {
  if (videoWidth <= 0 || videoHeight <= 0 || element.width <= 0 || element.height <= 0) return element;
  const scale = Math.min(element.width / videoWidth, element.height / videoHeight);
  const width = videoWidth * scale;
  const height = videoHeight * scale;
  return {
    left: element.left + (element.width - width) / 2,
    top: element.top + (element.height - height) / 2,
    width,
    height,
  };
}

/** A client position as 0..65535 over the picture, clamped to its edges so dragging off the picture still works. */
export function normalized(box: Box, clientX: number, clientY: number): { x: number; y: number } {
  const along = (v: number, from: number, size: number) => {
    if (size <= 1) return 0;
    const t = Math.min(1, Math.max(0, (v - from) / (size - 1)));
    return Math.round(t * 65535);
  };
  return { x: along(clientX, box.left, box.width), y: along(clientY, box.top, box.height) };
}

/** Browser button numbers (`MouseEvent.button`) to protocol buttons. */
export function buttonOf(button: number): MouseButton | undefined {
  return (['left', 'middle', 'right', 'back', 'forward'] as const)[button];
}

/** Wheel units of 120 per step from a WheelEvent delta; the browser's `deltaY > 0` is down, the protocol's is up. */
export function wheelUnits(delta: number, deltaMode: number): number {
  // Pixels: Chrome scrolls 100 px per notch on Windows. Lines: 3 lines per notch. Pages: one notch.
  const perNotch = deltaMode === 0 ? 100 : deltaMode === 1 ? 3 : 1;
  return (-delta / perNotch) * 120;
}

/** Splits accumulated wheel units into whole units to send now and the remainder to keep. */
export function takeWhole(acc: number): [number, number] {
  const whole = Math.trunc(Math.max(-32768, Math.min(32767, acc)));
  return [whole, acc - whole];
}
