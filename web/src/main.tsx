import { render } from 'preact';
import { Root } from './app/Root';
import './app/app.css';

const root = document.getElementById('app');
if (root) {
  render(<Root />, root);
}
