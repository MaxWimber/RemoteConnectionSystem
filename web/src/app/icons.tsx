// Line icons of the panel (D-050): one style for every button, drawn here on a 24×24 grid, stroke in the text colour.
// They replace emoji, which every phone and browser draws differently.

export type IconName =
  | 'menu'
  | 'fullscreen'
  | 'secure'
  | 'keyboard'
  | 'gamepad'
  | 'mouse'
  | 'files'
  | 'sound-on'
  | 'sound-waiting'
  | 'sound-off'
  | 'paste'
  | 'settings'
  | 'download'
  | 'sign-out'
  | 'close';

const speaker = 'M4 9.5v5h4l5 4v-13l-5 4z';

const paths: Record<IconName, string[]> = {
  menu: ['M4 7h16', 'M4 12h16', 'M4 17h16'],
  fullscreen: ['M4 9V4h5', 'M15 4h5v5', 'M20 15v5h-5', 'M9 20H4v-5'],
  secure: ['M6 11h12v9H6z', 'M8.5 11V8a3.5 3.5 0 0 1 7 0v3', 'M12 15v2'],
  keyboard: ['M3 6.5h18v11H3z', 'M7 10h.01', 'M10.5 10h.01', 'M14 10h.01', 'M17 10h.01', 'M7.5 14h9'],
  gamepad: [
    'M7 8h10a4 4 0 0 1 4 4v2.5a2.5 2.5 0 0 1-4.6 1.4L15 14H9l-1.4 1.9A2.5 2.5 0 0 1 3 14.5V12a4 4 0 0 1 4-4z',
    'M8 10.5v3',
    'M6.5 12h3',
    'M15.5 11h.01',
    'M17 13h.01',
  ],
  mouse: ['M12 3a5 5 0 0 1 5 5v8a5 5 0 0 1-10 0V8a5 5 0 0 1 5-5z', 'M12 7v3'],
  files: ['M3.5 6.5a1.5 1.5 0 0 1 1.5-1.5h4l2 2h8a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1-1.5 1.5H5a1.5 1.5 0 0 1-1.5-1.5z'],
  'sound-on': [speaker, 'M16 9.5a3.5 3.5 0 0 1 0 5', 'M18.5 7a7 7 0 0 1 0 10'],
  'sound-waiting': [speaker, 'M16 9.5a3.5 3.5 0 0 1 0 5'],
  'sound-off': [speaker, 'M16.5 9.5l5 5', 'M21.5 9.5l-5 5'],
  paste: ['M7 5H5.5v15.5h13V5H17', 'M9 3.5h6v3H9z', 'M8.5 11h7', 'M8.5 15h5'],
  settings: ['M4 7h9', 'M17 7h3', 'M4 17h3', 'M11 17h9', 'M15 5v4', 'M9 15v4'],
  download: ['M12 4v11', 'M7.5 10.5L12 15l4.5-4.5', 'M5 19.5h14'],
  'sign-out': ['M10 4.5H5.5v15H10', 'M14.5 8l4 4-4 4', 'M18.5 12H9'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
};

export function Icon({ name }: { name: IconName }) {
  return (
    <svg
      class="icon"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      {paths[name].map((d) => (
        <path key={d} d={d} />
      ))}
    </svg>
  );
}
