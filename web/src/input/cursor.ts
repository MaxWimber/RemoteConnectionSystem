// The server's pointer, drawn by the browser (P4-7): Desktop Duplication leaves it out of the picture.
//
// Without Pointer Lock the shape becomes the CSS cursor of the video, so it follows the local mouse with no delay.
// With Pointer Lock the local pointer is hidden and the shape is drawn over the video where the server says.

import { contentBox, type Box } from './pointer';
import type { ServerMessage } from './protocol';

/** Browsers ignore CSS cursors larger than this; bigger shapes are drawn over the video instead. */
const CSS_CURSOR_MAX = 128;

export interface Shape {
  url: string;
  width: number;
  height: number;
  hotX: number;
  hotY: number;
}

/** Where to draw a shape over the picture: the server position in 0..65535, the shape scaled like the video. */
export function overlayPlacement(
  box: Box,
  videoWidth: number,
  x: number,
  y: number,
  shape: Pick<Shape, 'width' | 'height' | 'hotX' | 'hotY'>,
): { left: number; top: number; width: number; height: number } {
  const scale = videoWidth > 0 ? box.width / videoWidth : 1;
  const px = box.left + (x / 65535) * (box.width - 1);
  const py = box.top + (y / 65535) * (box.height - 1);
  return {
    left: px - shape.hotX * scale,
    top: py - shape.hotY * scale,
    width: shape.width * scale,
    height: shape.height * scale,
  };
}

/** The CSS `cursor` value for a shape, or null when the browser would not show it. */
export function cssCursor(shape: Shape | null, visible: boolean): string {
  if (!visible) return 'none';
  if (!shape) return 'default';
  if (shape.width > CSS_CURSOR_MAX || shape.height > CSS_CURSOR_MAX) return 'none';
  return `url("${shape.url}") ${shape.hotX} ${shape.hotY}, default`;
}

/** How the pointer is shown: CSS cursor on a PC, over the video under Pointer Lock, where the phone's finger put it. */
export type CursorMode = 'css' | 'locked' | 'touchpad';

/** A plain arrow until the server sends a shape. */
const ARROW: Shape = {
  url:
    'data:image/svg+xml,' +
    encodeURIComponent(
      '<svg xmlns="http://www.w3.org/2000/svg" width="12" height="19" viewBox="0 0 12 19">' +
        '<path d="M.5.5v15l4-4 3 7 2-1-3-7h5.5z" fill="#fff" stroke="#000"/></svg>',
    ),
  width: 12,
  height: 19,
  hotX: 0,
  hotY: 0,
};

export class CursorView {
  private shape: Shape | null = null;
  private visible = true;
  private x = 0;
  private y = 0;
  private positions = 0;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly overlay: HTMLImageElement,
    private readonly mode: () => CursorMode,
    /** The position the phone's touchpad keeps, 0..65535; it does not wait for the server. */
    private readonly local: () => { x: number; y: number } | null = () => null,
  ) {}

  /** The last position the server reported, or null before the first one. */
  get serverPosition(): { x: number; y: number } | null {
    return this.positions > 0 ? { x: this.x, y: this.y } : null;
  }

  handle(m: ServerMessage): void {
    if (m.type === 'cursor_shape') {
      if (this.shape) URL.revokeObjectURL(this.shape.url);
      const url = URL.createObjectURL(new Blob([m.png as BlobPart], { type: 'image/png' }));
      this.shape = { url, width: m.width, height: m.height, hotX: m.hot_x, hotY: m.hot_y };
    } else {
      this.positions += 1;
      this.visible = m.visible;
      this.x = m.x;
      this.y = m.y;
    }
    this.render();
  }

  /** For the statistics panel: positions received and the current shape. */
  describe(): string {
    const shape = this.shape ? `${this.shape.width}×${this.shape.height}` : 'нет';
    return `курсор: форма ${shape}, положений ${this.positions}${this.visible ? '' : ', на сервере скрыт'}`;
  }

  render(): void {
    const mode = this.mode();
    const big = this.shape !== null && (this.shape.width > CSS_CURSOR_MAX || this.shape.height > CSS_CURSOR_MAX);
    // On a PC the pointer follows the local mouse and is always shown: Windows may hide its own after touch
    // input and not bring it back for injected mouse moves. Under Pointer Lock a game decides.
    this.video.style.cursor = mode === 'css' && !big ? cssCursor(this.shape, true) : 'none';
    let at: { x: number; y: number } | null = null;
    if (mode === 'touchpad') at = this.local() ?? this.serverPosition ?? { x: 32768, y: 32768 };
    else if ((mode === 'locked' || big) && this.visible) at = this.serverPosition;
    this.overlay.hidden = at === null;
    if (!at) return;
    const shape = this.shape ?? ARROW;
    if (this.overlay.getAttribute('src') !== shape.url) this.overlay.src = shape.url;
    const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
    const p = overlayPlacement(box, this.video.videoWidth, at.x, at.y, shape);
    const s = this.overlay.style;
    s.left = `${p.left}px`;
    s.top = `${p.top}px`;
    s.width = `${p.width}px`;
    s.height = `${p.height}px`;
  }
}
