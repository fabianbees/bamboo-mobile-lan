import { useState, useEffect, useRef } from 'react';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

interface RawMessage {
  id: number;
  ts: string;
  parsed: unknown;
  kind: 'mqtt' | 'ssdp' | 'error';
}

const KIND_STYLE = {
  mqtt: { box: 'bg-zinc-900 border-zinc-800', head: 'bg-zinc-800 border-zinc-700', id: 'text-zinc-500', tag: 'text-zinc-600', body: 'text-green-400' },
  ssdp: { box: 'bg-blue-950 border-blue-800', head: 'bg-blue-900 border-blue-800', id: 'text-blue-400 font-bold', tag: 'text-blue-400', body: 'text-blue-300' },
  error: { box: 'bg-red-950 border-red-800', head: 'bg-red-900 border-red-800', id: 'text-red-400 font-bold', tag: 'text-red-400', body: 'text-red-300' },
} as const;

let seq = 0;

export default function DebugPage({ onMenuOpen }: { onMenuOpen: () => void }) {
  const [messages, setMessages] = useState<RawMessage[]>([]);
  const [paused, setPaused] = useState(false);
  const pausedRef = useRef(false);
  const bottomRef = useRef<HTMLDivElement>(null);

  pausedRef.current = paused;

  useEffect(() => {
    const unsub = listen<string>('mqtt-raw', (e) => {
      if (pausedRef.current) return;
      let parsed: unknown;
      try { parsed = JSON.parse(e.payload); } catch { parsed = e.payload; }
      setMessages((prev) => [...prev.slice(-99), { id: ++seq, ts: new Date().toLocaleTimeString(), parsed, kind: 'mqtt' as const }]);
    });
    const unsubSsdp = listen<string>('ssdp-debug', (e) => {
      if (pausedRef.current) return;
      setMessages((prev) => [...prev.slice(-99), { id: ++seq, ts: new Date().toLocaleTimeString(), parsed: e.payload, kind: 'ssdp' as const }]);
    });
    const unsubError = listen<string>('mqtt-error', (e) => {
      if (pausedRef.current) return;
      setMessages((prev) => [...prev.slice(-99), { id: ++seq, ts: new Date().toLocaleTimeString(), parsed: e.payload, kind: 'error' as const }]);
    });
    return () => { unsub.then((f) => f()); unsubSsdp.then((f) => f()); unsubError.then((f) => f()); };
  }, []);

  useEffect(() => {
    if (!paused) bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages, paused]);

  return (
    <div className='min-h-screen bg-zinc-950 text-white flex flex-col'>
      <div
        className='sticky top-0 z-10 flex items-center justify-between px-4 pb-3 bg-zinc-900 border-b border-zinc-800 shrink-0'
        style={{ paddingTop: 'calc(env(safe-area-inset-top, 0px) + 0.75rem)' }}>
        <button onClick={onMenuOpen} className='text-zinc-400 hover:text-white transition-colors' aria-label='Menu'>
          <svg className='w-6 h-6' fill='none' viewBox='0 0 24 24' stroke='currentColor' strokeWidth={2}>
            <path strokeLinecap='round' strokeLinejoin='round' d='M4 6h16M4 12h16M4 18h16' />
          </svg>
        </button>
        <h1 className='font-semibold text-lg'>MQTT Debug</h1>
        <div className='flex items-center gap-2'>
          <button
            onClick={() => invoke('debug_send_request', { payload: JSON.stringify({ pushing: { sequence_id: '0', command: 'pushall', version: 1 } }) }).catch(() => {})}
            className='text-xs font-mono px-2 py-1 rounded bg-teal-800 text-teal-200 hover:bg-teal-700 transition-colors'>
            Pushall
          </button>
          <button
            onClick={() => invoke('debug_send_request', { payload: JSON.stringify({ info: { sequence_id: '0', command: 'get_version' } }) }).catch(() => {})}
            className='text-xs font-mono px-2 py-1 rounded bg-teal-800 text-teal-200 hover:bg-teal-700 transition-colors'>
            get_version
          </button>
          <button
            onClick={() => setPaused((p) => !p)}
            className={`text-xs font-mono px-2 py-1 rounded transition-colors ${
              paused ? 'bg-yellow-700 text-yellow-200' : 'bg-zinc-700 text-zinc-300 hover:bg-zinc-600'
            }`}>
            {paused ? 'Resume' : 'Pause'}
          </button>
          <button
            onClick={() => setMessages([])}
            className='text-xs font-mono px-2 py-1 rounded bg-zinc-700 text-zinc-300 hover:bg-zinc-600 transition-colors'>
            Clear
          </button>
        </div>
      </div>

      <div className='flex-1 overflow-y-auto p-3 flex flex-col gap-2 font-mono text-xs'>
        {messages.length === 0 && (
          <p className='text-zinc-600 text-center py-12'>Waiting for MQTT messages…</p>
        )}
        {messages.map((msg) => (
          <div key={msg.id} className={`border rounded-lg overflow-hidden ${KIND_STYLE[msg.kind].box}`}>
            <div className={`flex items-center gap-2 px-3 py-1.5 border-b ${KIND_STYLE[msg.kind].head}`}>
              <span className={KIND_STYLE[msg.kind].id}>#{msg.id}</span>
              <span className='text-zinc-400'>{msg.ts}</span>
              <span className={`ml-auto text-xs ${KIND_STYLE[msg.kind].tag}`}>
                {msg.kind === 'ssdp' ? 'SSDP' : msg.kind === 'error' ? 'ERROR' : topLevelKey(msg.parsed)}
              </span>
            </div>
            <pre className={`p-3 overflow-x-auto whitespace-pre-wrap break-all leading-relaxed ${KIND_STYLE[msg.kind].body}`}>
              {typeof msg.parsed === 'string' ? msg.parsed : JSON.stringify(msg.parsed, null, 2)}
            </pre>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>
    </div>
  );
}

function topLevelKey(parsed: unknown): string {
  if (parsed && typeof parsed === 'object') {
    return Object.keys(parsed as object).join(', ');
  }
  return '';
}
