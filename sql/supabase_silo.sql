-- Spectr Vision / SiloMonitor — single event stream.
-- Dashboard: https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new
--
-- Keep ONLY public.silo_events for this app.
-- Do NOT drop Spectr OS tables on this project:
--   spectr_folders, spectr_alerts, spectr_cloud_nodes, spectr_workflows
-- (spectr_folders holds digitwin / files data.)

-- Leftover names from an earlier silo schema (safe if already gone).
drop table if exists silo_alerts cascade;
drop table if exists silo_checks cascade;
drop table if exists silo_stats cascade;

create table if not exists silo_events (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  -- empty_alert | check | stats
  kind text not null,
  empty boolean,
  confidence real,
  checks bigint,
  empty_hits bigint,
  alerts_sent bigint,
  labels_empty bigint,
  labels_full bigint,
  created_at timestamptz not null default now()
);

create index if not exists silo_events_site_ts on silo_events (site_id, ts desc);
create index if not exists silo_events_kind on silo_events (kind, ts desc);

alter table silo_events enable row level security;

drop policy if exists "anon insert events" on silo_events;
drop policy if exists "anon select events" on silo_events;

create policy "anon insert events" on silo_events
  for insert to anon, authenticated with check (true);
create policy "anon select events" on silo_events
  for select to anon, authenticated using (true);

grant usage on schema public to anon, authenticated;
grant insert, select on table public.silo_events to anon, authenticated;

do $$
declare
  seq text;
begin
  seq := pg_get_serial_sequence('public.silo_events', 'id');
  if seq is not null then
    execute format('grant usage, select on sequence %s to anon, authenticated', seq);
  end if;
end $$;

notify pgrst, 'reload schema';
