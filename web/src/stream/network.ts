// Network and visibility changes for the diagnostics log: they explain dropped sockets on phones.

interface NetworkInformationLike extends EventTarget {
  type?: string;
  effectiveType?: string;
  downlink?: number;
  rtt?: number;
}

export function describeConnection(c: NetworkInformationLike | undefined): string {
  if (!c) return 'нет данных';
  const parts = [c.type, c.effectiveType].filter((p): p is string => typeof p === 'string' && p.length > 0);
  if (typeof c.downlink === 'number') parts.push(`${c.downlink} Мбит/с`);
  if (typeof c.rtt === 'number') parts.push(`rtt ${c.rtt} мс`);
  return parts.length > 0 ? parts.join(', ') : 'нет данных';
}

/** Logs network type changes, online and offline, and the page going hidden or visible. Returns the unsubscribe. */
export function watchNetwork(log: (message: string) => void): () => void {
  const connection = (navigator as Navigator & { connection?: NetworkInformationLike }).connection;
  const onChange = () => log(`сеть: ${describeConnection(connection)}`);
  const onOnline = () => log('сеть: online');
  const onOffline = () => log('сеть: offline');
  const onVisibility = () => log(`страница: ${document.visibilityState}`);
  connection?.addEventListener('change', onChange);
  window.addEventListener('online', onOnline);
  window.addEventListener('offline', onOffline);
  document.addEventListener('visibilitychange', onVisibility);
  return () => {
    connection?.removeEventListener('change', onChange);
    window.removeEventListener('online', onOnline);
    window.removeEventListener('offline', onOffline);
    document.removeEventListener('visibilitychange', onVisibility);
  };
}
