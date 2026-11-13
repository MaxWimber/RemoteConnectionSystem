// The panel of buttons over the picture (P11-2). Folded it is one small handle, so it covers almost nothing of the
// remote screen: by default in the black bar beside the picture, or at the top in the middle. A tap opens the
// buttons towards the middle of the screen, a button or another tap on the handle folds them again. The handle can be
// dragged anywhere, and the place is remembered on this browser.

import type { ComponentChildren, RefObject } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';

import { contentBox } from '../input/pointer';
import { HANDLE_SIZE, handleCenter, loadPlace, savePlace, type Place } from './toolbar-place';

/** A move this long in CSS pixels turns a tap on the handle into a drag. */
const DRAG_START = 8;

interface Props {
  label: string;
  video: RefObject<HTMLVideoElement>;
  children: ComponentChildren;
}

export function Toolbar({ label, video, children }: Props) {
  const [open, setOpen] = useState(false);
  const [place, setPlace] = useState<Place | null>(() => loadPlace());
  const [, setLayout] = useState(0);
  const drag = useRef<{ id: number; x: number; y: number; moved: boolean } | null>(null);
  const dragged = useRef(false);

  // The default place follows the picture: a new video size or a turned phone moves the black bars.
  useEffect(() => {
    const relayout = () => setLayout((n) => n + 1);
    const v = video.current;
    window.addEventListener('resize', relayout);
    v?.addEventListener('resize', relayout);
    v?.addEventListener('loadedmetadata', relayout);
    return () => {
      window.removeEventListener('resize', relayout);
      v?.removeEventListener('resize', relayout);
      v?.removeEventListener('loadedmetadata', relayout);
    };
  }, [video]);

  const screen = { width: window.innerWidth, height: window.innerHeight };
  const v = video.current;
  const picture = v
    ? contentBox(v.getBoundingClientRect(), v.videoWidth, v.videoHeight)
    : { left: 0, top: 0, ...screen };
  const center = handleCenter(screen, picture, place);
  const toLeft = center.x > screen.width / 2;
  const below = center.y < screen.height / 2;
  const room = Math.max(HANDLE_SIZE, (toLeft ? center.x : screen.width - center.x) - HANDLE_SIZE);

  const onDown = (e: PointerEvent) => {
    drag.current = { id: e.pointerId, x: e.clientX, y: e.clientY, moved: false };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  };
  const onMove = (e: PointerEvent) => {
    const d = drag.current;
    if (!d || d.id !== e.pointerId) return;
    if (!d.moved && Math.hypot(e.clientX - d.x, e.clientY - d.y) < DRAG_START) return;
    d.moved = true;
    setPlace({ x: e.clientX / screen.width, y: e.clientY / screen.height });
  };
  const onUp = (e: PointerEvent) => {
    const d = drag.current;
    if (!d || d.id !== e.pointerId) return;
    drag.current = null;
    if (d.moved) {
      dragged.current = true;
      const c = handleCenter(screen, picture, { x: e.clientX / screen.width, y: e.clientY / screen.height });
      const kept = { x: c.x / screen.width, y: c.y / screen.height };
      setPlace(kept);
      savePlace(kept);
    }
  };
  const onClick = () => {
    // The click that ends a drag does not open the panel.
    if (dragged.current) {
      dragged.current = false;
      return;
    }
    setOpen(!open);
  };

  return (
    <div class="toolbar" aria-label={label} style={{ left: `${center.x}px`, top: `${center.y}px` }}>
      <button
        class={open ? 'toolbar-handle open' : 'toolbar-handle'}
        title={open ? 'Свернуть панель' : 'Панель (можно перетащить)'}
        aria-expanded={open}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={() => (drag.current = null)}
        onClick={onClick}
      >
        ☰
      </button>
      {open && (
        <div
          class="toolbar-buttons"
          style={{
            [toLeft ? 'right' : 'left']: `${HANDLE_SIZE / 2 + 6}px`,
            [below ? 'top' : 'bottom']: `-${HANDLE_SIZE / 2}px`,
            maxWidth: `${room}px`,
            flexDirection: toLeft ? 'row-reverse' : 'row',
          }}
          // A button did its work: fold the panel so it does not cover the screen.
          onClick={() => setOpen(false)}
        >
          {children}
        </div>
      )}
    </div>
  );
}
