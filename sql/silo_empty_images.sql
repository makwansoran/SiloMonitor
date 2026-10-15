-- Alert still path on silo_empty + allow the Pi to backfill NULL rows.
-- Run once in the Supabase SQL Editor:
-- https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new
--
-- Each empty alert row keeps:
--   ts          — date + time the alert was sent (timestamptz)
--   image_path  — JPEG in silo-frames, e.g. spectr-pi/alerts/1791198842.jpg

alter table public.silo_empty
  add column if not exists image_path text;

comment on column public.silo_empty.image_path is
  'Object path in silo-frames bucket for the alert JPEG';

create index if not exists silo_empty_site_ts
  on public.silo_empty (site_id, ts desc);

-- Pi uses the publishable key: insert/select only was not enough to fill
-- image_path on rows created before that column was sent.
grant update on table public.silo_empty to anon, authenticated;

drop policy if exists "anon update" on public.silo_empty;
create policy "anon update" on public.silo_empty
  for update to anon, authenticated
  using (site_id in (select site_id from silo_allowed_sites))
  with check (site_id in (select site_id from silo_allowed_sites));

-- Backfill from the Storage naming convention {site}/alerts/{unix}.jpg
update public.silo_empty
set image_path = site_id || '/alerts/' || floor(extract(epoch from ts))::bigint || '.jpg'
where image_path is null
  and site_id is not null
  and ts is not null;

-- Drop the accidental probe row (safe if absent).
delete from public.silo_empty
where ts = '2099-01-01T00:00:00+00:00'
  and image_path = 'spectr-pi/alerts/test.jpg';

notify pgrst, 'reload schema';
