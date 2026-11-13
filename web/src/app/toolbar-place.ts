// Where the folded panel's handle sits over the screen (P11-2). Pure, so tests check it.

import type { Box } from '../input/pointer';

/** The handle's center as fractions of the screen, when the user dragged it there. */
export interface Place {
  x: number;
  y: number;
}

/** The handle's side in CSS pixels, and the gap it keeps from the edges. */
export const HANDLE_SIZE = 36;
export const EDGE_GAP = 6;
const PLACE_KEY = 'rcs.toolbarPlace';

const fraction = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v) && v >= 0 && v <= 1;

export function loadPlace(): Place | null {
  try {
    const v = JSON.parse(localStorage.getItem(PLACE_KEY) ?? 'null') as Partial<Place> | null;
    return v && fraction(v.x) && fraction(v.y) ? { x: v.x, y: v.y } : null;
  } catch {
    return null;
  }
}

export function savePlace(place: Place): void {
  try {
    localStorage.setItem(PLACE_KEY, JSON.stringify(place));
  } catch {
    // Without storage the place lasts until the page is closed.
  }
}

/** A center in pixels kept inside the screen, so the whole handle stays visible. */
export function clampCenter(screen: { width: number; height: number }, x: number, y: number): { x: number; y: number } {
  const half = HANDLE_SIZE / 2 + EDGE_GAP;
  const along = (v: number, size: number) => (size <= 2 * half ? size / 2 : Math.min(size - half, Math.max(half, v)));
  return { x: along(x, screen.width), y: along(y, screen.height) };
}

/**
 * Where the handle goes in pixels. A place the user chose wins. Otherwise it sits in the black bar beside or above
 * the picture when one is wide enough, so it covers nothing of the remote screen; without a bar, at the top in the
 * middle, where windows have their title and no buttons.
 */
export function handleCenter(
  screen: { width: number; height: number },
  picture: Box,
  place: Place | null,
): { x: number; y: number } {
  if (place) return clampCenter(screen, place.x * screen.width, place.y * screen.height);
  const need = HANDLE_SIZE + 2 * EDGE_GAP;
  const right = screen.width - (picture.left + picture.width);
  if (right >= need) return clampCenter(screen, screen.width - right / 2, 0);
  const top = picture.top;
  if (top >= need) return clampCenter(screen, screen.width, top / 2);
  return clampCenter(screen, screen.width / 2, 0);
}
