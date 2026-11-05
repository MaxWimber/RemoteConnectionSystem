import { useEffect, useState } from 'preact/hooks';
import { me } from '../api/auth';
import { App } from './App';
import { SignIn, type SignInStep } from './SignIn';

type View = { kind: 'checking' } | { kind: 'sign-in'; step: SignInStep; notice?: string } | { kind: 'stream' };

/** Sign-in first, the stream only with a full session (docs/SECURITY.md). */
export function Root() {
  const base = import.meta.env.BASE_URL;
  const [view, setView] = useState<View>({ kind: 'checking' });

  const check = async (notice?: string) => {
    try {
      const m = await me(base);
      if (m.state === 'signed-in') setView({ kind: 'stream' });
      else setView({ kind: 'sign-in', step: m.state === 'totp' ? 'totp' : 'password', notice });
    } catch (e) {
      setView({ kind: 'sign-in', step: 'password', notice: `Сервер не отвечает: ${String(e)}` });
    }
  };

  useEffect(() => {
    void check();
  }, []);

  switch (view.kind) {
    case 'checking':
      return <div class="signin" />;
    case 'sign-in':
      return (
        <SignIn
          key={`${view.step}:${view.notice ?? ''}`}
          base={base}
          step={view.step}
          notice={view.notice}
          onSignedIn={() => setView({ kind: 'stream' })}
        />
      );
    case 'stream':
      return <App onSignedOut={(reason) => void check(reason)} />;
  }
}
