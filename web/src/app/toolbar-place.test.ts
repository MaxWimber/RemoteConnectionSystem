import { describe, expect, it } from 'vitest';

import { EDGE_GAP, HANDLE_SIZE, clampCenter, handleCenter } from './toolbar-place';

const half = HANDLE_SIZE / 2 + EDGE_GAP;

describe('the panel handle', () => {
  it('sits in the black bar beside the picture on a wide phone screen', () => {
    // A 20:9 phone in landscape with a 16:9 picture: bars of 88 px on both sides.
    const screen = { width: 900, height: 405 };
    const picture = { left: 90, top: 0, width: 720, height: 405 };
    expect(handleCenter(screen, picture, null)).toEqual({ x: 855, y: half });
  });

  it('sits in the bar above the picture in portrait', () => {
    const screen = { width: 375, height: 812 };
    const picture = { left: 0, top: 300, width: 375, height: 211 };
    expect(handleCenter(screen, picture, null)).toEqual({ x: 375 - half, y: 150 });
  });

  it('goes to the top middle when the picture fills the screen', () => {
    const screen = { width: 1920, height: 1080 };
    expect(handleCenter(screen, { left: 0, top: 0, width: 1920, height: 1080 }, null)).toEqual({ x: 960, y: half });
  });

  it('keeps a chosen place, inside the screen', () => {
    const screen = { width: 800, height: 400 };
    const picture = { left: 0, top: 0, width: 800, height: 400 };
    expect(handleCenter(screen, picture, { x: 0.25, y: 0.5 })).toEqual({ x: 200, y: 200 });
    expect(handleCenter(screen, picture, { x: 1, y: 0 })).toEqual({ x: 800 - half, y: half });
    expect(clampCenter({ width: 20, height: 20 }, 0, 0)).toEqual({ x: 10, y: 10 });
  });
});
