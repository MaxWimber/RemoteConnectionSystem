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

export class CursorView {
  private shape: Shape | null = null;
  private visible = true;
  private x = 0;
  private y = 0;
  private positions = 0;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly overlay: HTMLImageElement,
    private readonly locked: () => boolean,
  ) {}

  handle(m: ServerMessage): void {
    if (m.type === 'cursor_shape') {
      if (this.shape) URL.revokeObjectURL(this.shape.url);
      const url = URL.createObjectURL(new Blob([m.png as BlobPart], { type: 'image/png' }));
      this.shape = { url, width: m.width, height: m.height, hotX: m.hot_x, hotY: m.hot_y };
      this.overlay.src = url;
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
    return `курсор: форма ${shape}, положений ${this.positions}${this.visible ? '' : ', скрыт'}`;
  }

  render(): void {
    const locked = this.locked();
    const big = this.shape !== null && (this.shape.width > CSS_CURSOR_MAX || this.shape.height > CSS_CURSOR_MAX);
    this.video.style.cursor = locked ? 'none' : cssCursor(this.shape, this.visible);
    const overlay = (locked || big) && this.visible && this.shape !== null;
    this.overlay.hidden = !overlay;
    if (!overlay || !this.shape) return;
    const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
    const p = overlayPlacement(box, this.video.videoWidth, this.x, this.y, this.shape);
    const s = this.overlay.style;
    s.left = `${p.left}px`;
    s.top = `${p.top}px`;
    s.width = `${p.width}px`;
    s.height = `${p.height}px`;
  }
}
