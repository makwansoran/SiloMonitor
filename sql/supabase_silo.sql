-- SiloMonitor tables. One fact per table. Nothing is mixed.
-- Dashboard: https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new
--
-- Kept:        silo_empty, silo_full, silo_radio_tx, silo_radio_fail
-- 30 days:     silo_heartbeat, silo_power, silo_camera_up, silo_camera_down,
--              silo_app_start, silo_armed, silo_disarmed
--
-- silo_events is the old mixed inbox. Rows are copied across, then left in place.
-- Do NOT drop Spectr OS tables on this project:
--   spectr_folders, spectr_alerts, spectr_cloud_nodes, spectr_workflows

-- Leftover names from an earlier silo schema (safe if already gone).
drop table if exists silo_alerts cascade;
drop table if exists silo_checks cascade;
drop table if exists silo_stats cascade;

create table if not exists silo_allowed_sites (
  site_id text primary key
);

insert into silo_allowed_sites (site_id)
values ('spectr-pi')
on conflict (site_id) do nothing;

create table if not exists silo_empty (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  created_at timestamptz not null default now()
);

create table if not exists silo_full (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  confidence real,
  created_at timestamptz not null default now()
);

create table if not exists silo_radio_tx (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  created_at timestamptz not null default now()
);

create table if not exists silo_radio_fail (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  created_at timestamptz not null default now()
);

create table if not exists silo_heartbeat (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  checks bigint,
  empty_hits bigint,
  alerts_sent bigint,
  labels_empty bigint,
  labels_full bigint,
  created_at timestamptz not null default now()
);

create table if not exists silo_power (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  throttled bigint not null default 0,
  created_at timestamptz not null default now()
);

create table if not exists silo_camera_up (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  created_at timestamptz not null default now()
);

create table if not exists silo_camera_down (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  created_at timestamptz not null default now()
);

create table if not exists silo_app_start (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  created_at timestamptz not null default now()
);

create table if not exists silo_armed (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  created_at timestamptz not null default now()
);

create table if not exists silo_disarmed (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  empty boolean,
  created_at timestamptz not null default now()
);

do $$
declare
  rel text;
  kept text[] := array[
    'silo_empty', 'silo_full', 'silo_radio_tx', 'silo_radio_fail'
  ];
  short text[] := array[
    'silo_heartbeat', 'silo_power', 'silo_camera_up', 'silo_camera_down',
    'silo_app_start', 'silo_armed', 'silo_disarmed'
  ];
  allrel text[] := kept || short;
  seq text;
begin
  alter table silo_allowed_sites enable row level security;
  drop policy if exists "anon select sites" on silo_allowed_sites;
  create policy "anon select sites" on silo_allowed_sites
    for select to anon, authenticated
    using (true);

  foreach rel in array allrel loop
    execute format('create index if not exists %I on %I (site_id, ts desc)', rel || '_site_ts', rel);
    execute format('alter table %I enable row level security', rel);
    execute format('drop policy if exists "anon insert" on %I', rel);
    execute format('drop policy if exists "anon select" on %I', rel);
    execute format('drop policy if exists "anon delete old" on %I', rel);
    execute format(
      'create policy "anon insert" on %I for insert to anon, authenticated with check (site_id in (select site_id from silo_allowed_sites))',
      rel
    );
    execute format(
      'create policy "anon select" on %I for select to anon, authenticated using (site_id in (select site_id from silo_allowed_sites))',
      rel
    );
    execute format('grant insert, select on table public.%I to anon, authenticated', rel);
    seq := pg_get_serial_sequence('public.' || rel, 'id');
    if seq is not null then
      execute format('grant usage, select on sequence %s to anon, authenticated', seq);
    end if;
  end loop;

  foreach rel in array short loop
    execute format(
      'create policy "anon delete old" on %I for delete to anon, authenticated using (site_id in (select site_id from silo_allowed_sites) and ts < now() - interval ''30 days'')',
      rel
    );
    execute format('grant delete on table public.%I to anon, authenticated', rel);
  end loop;
end $$;

grant usage on schema public to anon, authenticated;
grant select on table public.silo_allowed_sites to anon, authenticated;

-- Copy the old mixed inbox once. Safe to run again. `check` rows are not copied.
do $$
begin
  if to_regclass('public.silo_events') is null then
    return;
  end if;

  insert into silo_empty (site_id, ts)
  select e.site_id, e.ts from silo_events e
  where e.kind = 'empty_alert'
    and not exists (select 1 from silo_empty n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_full (site_id, ts, confidence)
  select e.site_id, e.ts, e.confidence from silo_events e
  where e.kind = 'full'
    and not exists (select 1 from silo_full n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_radio_tx (site_id, ts, empty)
  select e.site_id, e.ts, e.empty from silo_events e
  where e.kind = 'radio_tx'
    and not exists (select 1 from silo_radio_tx n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_radio_fail (site_id, ts, empty)
  select e.site_id, e.ts, e.empty from silo_events e
  where e.kind = 'radio_fail'
    and not exists (select 1 from silo_radio_fail n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_heartbeat (site_id, ts, empty, checks, empty_hits, alerts_sent, labels_empty, labels_full)
  select e.site_id, e.ts, e.empty, e.checks, e.empty_hits, e.alerts_sent, e.labels_empty, e.labels_full
  from silo_events e
  where e.kind = 'heartbeat'
    and not exists (select 1 from silo_heartbeat n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_power (site_id, ts, throttled)
  select e.site_id, e.ts, coalesce(e.checks, 0) from silo_events e
  where e.kind = 'power'
    and not exists (select 1 from silo_power n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_camera_up (site_id, ts)
  select e.site_id, e.ts from silo_events e
  where e.kind = 'camera_up'
    and not exists (select 1 from silo_camera_up n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_camera_down (site_id, ts)
  select e.site_id, e.ts from silo_events e
  where e.kind = 'camera_down'
    and not exists (select 1 from silo_camera_down n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_app_start (site_id, ts, empty)
  select e.site_id, e.ts, e.empty from silo_events e
  where e.kind = 'app_start'
    and not exists (select 1 from silo_app_start n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_armed (site_id, ts, empty)
  select e.site_id, e.ts, e.empty from silo_events e
  where e.kind = 'armed'
    and not exists (select 1 from silo_armed n where n.site_id = e.site_id and n.ts = e.ts);

  insert into silo_disarmed (site_id, ts, empty)
  select e.site_id, e.ts, e.empty from silo_events e
  where e.kind = 'disarmed'
    and not exists (select 1 from silo_disarmed n where n.site_id = e.site_id and n.ts = e.ts);
end $$;

-- The three mixed tables from the previous draft, if they were created.
drop table if exists silo_level cascade;
drop table if exists silo_radio cascade;
drop table if exists silo_health cascade;

notify pgrst, 'reload schema';

-- Live stills for the later cloud app (Pi uploads JPEG snapshots).
-- Paths: {site_id}/latest.jpg and {site_id}/alerts/{unix}.jpg
insert into storage.buckets (id, name, public, file_size_limit, allowed_mime_types)
values (
  'silo-frames',
  'silo-frames',
  false,
  5242880,
  array['image/jpeg', 'application/json']
)
on conflict (id) do update set
  file_size_limit = excluded.file_size_limit,
  allowed_mime_types = excluded.allowed_mime_types;

drop policy if exists "silo frames insert" on storage.objects;
drop policy if exists "silo frames update" on storage.objects;
drop policy if exists "silo frames select" on storage.objects;

create policy "silo frames insert" on storage.objects
  for insert to anon, authenticated
  with check (bucket_id = 'silo-frames');

create policy "silo frames update" on storage.objects
  for update to anon, authenticated
  using (bucket_id = 'silo-frames')
  with check (bucket_id = 'silo-frames');

create policy "silo frames select" on storage.objects
  for select to anon, authenticated
  using (bucket_id = 'silo-frames');
