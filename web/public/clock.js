// Server side of the screen-to-screen measurement (docs/HARDWARE.md): a millisecond counter drawn on every
// display refresh. A camera films this screen and the client screen together; the difference of the two
// readings is the delay. The bar moves one tenth of its width per 100 ms, so blurred digits can be checked.
const ms = document.getElementById('ms');
const frame = document.getElementById('frame');
const fill = document.getElementById('fill');
let n = 0;
function draw(now) {
  const t = Math.floor(now) % 100000;
  ms.textContent = String(t).padStart(5, '0');
  frame.textContent = 'кадр ' + n++;
  fill.style.left = ((Math.floor(now / 100) % 10) * 10) + '%';
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);
