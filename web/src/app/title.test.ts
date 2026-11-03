import { expect, test } from 'vitest';
import { appTitle } from './title';

test('uses the project name', () => {
  expect(appTitle).toBe('RemoteConnectionSystem');
});
