import { render } from 'preact';
import { appTitle } from './app/title';

const root = document.getElementById('app');
if (root) {
  render(<h1>{appTitle}</h1>, root);
}
