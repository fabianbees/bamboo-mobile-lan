import type { ExternalSpool as Spool } from '../vite-env';

export default function ExternalSpool({
  spool,
  selected,
  onSelect,
}: {
  spool: Spool;
  selected?: boolean;
  /** Absent when the spool cannot be loaded from the app. */
  onSelect?: () => void;
}) {
  const { tray, loaded, side } = spool;
  return (
    <button
      onClick={onSelect}
      disabled={!onSelect}
      className={`flex items-center w-full gap-4 text-left rounded-lg p-1 transition-colors ${
        selected ? 'ring-2 ring-teal-400 bg-teal-950/30' : ''
      }`}>
      <div className='relative shrink-0'>
        <div
          className={`w-10 h-10 rounded-lg border-2 transition-colors ${
            selected ? 'border-teal-400'
            : loaded ? 'border-amber-400'
            : 'border-zinc-700'
          }`}
          style={{ backgroundColor: tray.color ? `#${tray.color}` : '#3f3f46' }}
        />
        {loaded && (
          <span className='absolute -top-1 -right-1 w-2.5 h-2.5 rounded-full bg-amber-400 border border-zinc-900' />
        )}
      </div>

      <div className='flex flex-col gap-0.5 min-w-0'>
        <span className='text-zinc-400 text-xs'>
          External Spool{side && (side === 'left' ? ' · Left' : ' · Right')}
          {loaded && <span className='ml-1 text-amber-400'>· Loaded</span>}
        </span>
        <span className='text-white font-semibold text-sm'>
          {tray.tray_type}
        </span>
        {tray.name && (
          <span className='text-zinc-400 text-xs truncate'>{tray.name}</span>
        )}
      </div>
    </button>
  );
}
