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

create table if not exists silo_allowed_sites (
  site_id text primary key
);

insert into silo_allowed_sites (site_id)
values ('spectr-pi')
on conflict (site_id) do nothing;

create table if not exists silo_events (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  ts timestamptz not null default now(),
  -- empty_alert | heartbeat | camera_down | camera_up | radio_tx | radio_fail
  -- app_start | armed | disarmed
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
alter table silo_allowed_sites enable row level security;

drop policy if exists "anon insert events" on silo_events;
drop policy if exists "anon select events" on silo_events;
drop policy if exists "anon select sites" on silo_allowed_sites;

create policy "anon insert events" on silo_events
  for insert to anon, authenticated
  with check (site_id in (select site_id from silo_allowed_sites));

create policy "anon select events" on silo_events
  for select to anon, authenticated
  using (site_id in (select site_id from silo_allowed_sites));

create policy "anon select sites" on silo_allowed_sites
  for select to anon, authenticated
  using (true);

grant usage on schema public to anon, authenticated;
grant insert, select on table public.silo_events to anon, authenticated;
grant select on table public.silo_allowed_sites to anon, authenticated;

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

-- Live stills for the later cloud app (Pi uploads JPEG snapshots).
-- Paths: {site_id}/latest.jpg and {site_id}/alerts/{unix}.jpg
insert into storage.buckets (id, name, public, file_size_limit, allowed_mime_types)
values (
  'silo-frames',
  'silo-frames',
  false,
  5242880,
  array['image/jpeg']
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
