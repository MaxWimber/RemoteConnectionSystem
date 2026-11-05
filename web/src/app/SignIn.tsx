import { useEffect, useRef, useState } from 'preact/hooks';
import { errorText, login, recovery, totp, type AuthResult } from '../api/auth';
import { appTitle } from './title';

export type SignInStep = 'password' | 'totp' | 'recovery';

interface Props {
  base: string;
  step: SignInStep;
  notice?: string;
  onSignedIn(): void;
}

export function SignIn({ base, step: initial, notice, onSignedIn }: Props) {
  const [step, setStep] = useState<SignInStep>(initial);
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(notice ?? '');
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    setValue('');
    input.current?.focus();
  }, [step]);

  const submit = async (e: Event) => {
    e.preventDefault();
    if (busy || value.trim() === '') return;
    setBusy(true);
    let result: AuthResult;
    try {
      result =
        step === 'password' ? await login(base, value) : step === 'totp' ? await totp(base, value) : await recovery(base, value);
    } catch (err) {
      result = { ok: false, error: `network: ${String(err)}` };
    }
    setBusy(false);
    if (result.ok) {
      setError('');
      if (step === 'password') setStep('totp');
      else onSignedIn();
      return;
    }
    setError(errorText(result));
    setValue('');
    // An expired pending session means starting over with the password.
    if (step !== 'password' && result.error === 'unauthorized') setStep('password');
    input.current?.focus();
  };

  // Each step gets its own input: a reused one would keep the TOTP field's maxlength and pattern.
  const field =
    step === 'password' ? (
      <label key={step}>
        Пароль
        <input
          ref={input}
          type="password"
          autocomplete="current-password"
          value={value}
          onInput={(e) => setValue(e.currentTarget.value)}
        />
      </label>
    ) : step === 'totp' ? (
      <label key={step}>
        Код из приложения
        <input
          ref={input}
          type="text"
          inputMode="numeric"
          autocomplete="one-time-code"
          pattern="[0-9]{6}"
          maxLength={6}
          value={value}
          onInput={(e) => setValue(e.currentTarget.value.replace(/\D/g, ''))}
        />
      </label>
    ) : (
      <label key={step}>
        Код восстановления
        <input
          ref={input}
          type="text"
          autocomplete="off"
          autoCapitalize="characters"
          spellcheck={false}
          value={value}
          onInput={(e) => setValue(e.currentTarget.value)}
        />
      </label>
    );

  return (
    <div class="signin">
      <form class="signin-form" onSubmit={(e) => void submit(e)}>
        <h1>{appTitle}</h1>
        {field}
        {error && (
          <div class="signin-error" role="alert">
            {error}
          </div>
        )}
        <button type="submit" disabled={busy}>
          {busy ? 'Проверка…' : step === 'password' ? 'Далее' : 'Войти'}
        </button>
        {step === 'totp' && (
          <button type="button" class="link" onClick={() => setStep('recovery')}>
            Нет доступа к приложению — код восстановления
          </button>
        )}
        {step === 'recovery' && (
          <button type="button" class="link" onClick={() => setStep('totp')}>
            Ввести код из приложения
          </button>
        )}
      </form>
    </div>
  );
}
