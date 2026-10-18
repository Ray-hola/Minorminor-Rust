// Byte-based overall progress is explicitly an estimate; stage counts stay factual.
export function scanProgress(job, now = Date.now(), capacity = null) {
  const stats = job.stats || {};
  const stages = ['disk', 'projects', 'storage', 'models'];
  const stage = stats.stage || 'preparing';
  const status = job.status;
  const elapsedMs = Math.max(0, (Date.parse(job.finishedAt) || now) - (Date.parse(job.createdAt) || now));
  const idleMs = Math.max(0, now - (Date.parse(job.progressAt || job.createdAt) || now));
  const total = Number(stats.total) || 0;
  const completed = Number(stats.completed) || 0;
  const stageFraction = total > 0 ? Math.min(1, Math.max(0, completed / total)) : null;
  const estimate = Number(stats.estimatedBytes ?? stats.diskSnapshot?.used ?? capacity?.used);
  const isScan = job.kind === 'scan';
  let fraction = stageFraction;
  if (isScan) {
    const ranges = { preparing: [0, 0], disk: [0, .90], projects: [.90, .04], storage: [.94, .04], 'harness-analysis': [.98, .01], models: [.99, 0] };
    const range = ranges[stage];
    if (status === 'complete') fraction = 1;
    else if (range) {
      const part = stage === 'disk' ? Number.isFinite(estimate) && estimate > 0 ? Math.min(1, Math.max(0, Number(stats.bytes) || 0) / estimate) : null : stageFraction || 0;
      fraction = part == null ? null : Math.min(.995, range[0] + range[1] * part);
    } else fraction = null;
  }
  return {
    stage, status, stats, elapsedMs, idleMs,
    step: status === 'complete' ? 4 : Math.max(0, stages.indexOf(stage) + 1),
    steps: 4,
    fraction, stageFraction, estimated: isScan && status !== 'complete' && fraction != null,
    waiting: status === 'running' && idleMs >= 8000,
    error: status !== 'failed' ? '' : /database or disk is full|disk full|no space left/i.test(job.error || '')
      ? '磁盘空间不足，无法写入扫描索引。腾出空间后重试。' : job.error || '',
  };
}

// A later non-scan task can replace the persisted last-job record. The completed
// scan snapshot remains authoritative for the compact result after a restart.
export function progressJob(boot) {
  const jobs = boot?.jobs || [];
  const existing = jobs.find(j => j.status === 'running') || [...jobs].reverse().find(j => j.kind === 'scan');
  if (existing) return existing;
  const scan = boot?.scan;
  if (scan?.status !== 'complete') return null;
  const started = Date.parse(scan.createdAt);
  const elapsed = Math.max(0, Number(scan.stats?.elapsedMs) || 0);
  return {...scan, kind:'scan', finishedAt:Number.isFinite(started) ? new Date(started + elapsed).toISOString() : scan.createdAt};
}

export function scanScope(boot) {
  const job = [...(boot?.jobs || [])].reverse().find(j => j.kind === 'scan');
  const scope = job?.stats?.scanScope;
  if (scope === 'disk' || scope === 'home') return scope;
  return ['/', '/System/Volumes/Data'].includes(boot?.scan?.root) ? 'disk' : 'home';
}
