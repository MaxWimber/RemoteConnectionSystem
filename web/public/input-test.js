// The square flips between black and white on every key and button press, at once and without animation.
(function () {
  var square = document.getElementById('square');
  var count = document.getElementById('count');
  var white = false;
  var n = 0;
  function flip(e) {
    e.preventDefault();
    white = !white;
    n += 1;
    square.style.background = white ? '#fff' : '#000';
    count.textContent = 'Нажатий: ' + n;
  }
  window.addEventListener('keydown', flip);
  window.addEventListener('mousedown', flip);
})();
