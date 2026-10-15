-- Run this once in the Supabase SQL Editor if silo_reference_images is missing:
-- https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new

create table if not exists silo_reference_images (
  id bigint generated always as identity primary key,
  site_id text not null default 'spectr-pi',
  file_name text not null,
  label text not null check (label in ('empty', 'not_empty')),
  captured_ms bigint,
  bytes bigint,
  width integer,
  height integer,
  colour_path text not null,
  bw_path text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (site_id, file_name)
);

create index if not exists silo_reference_images_site_label
  on silo_reference_images (site_id, label, captured_ms desc);

alter table silo_reference_images enable row level security;

drop policy if exists "anon insert" on silo_reference_images;
drop policy if exists "anon select" on silo_reference_images;
drop policy if exists "anon update" on silo_reference_images;
drop policy if exists "anon delete" on silo_reference_images;

create policy "anon insert" on silo_reference_images
  for insert to anon, authenticated
  with check (site_id in (select site_id from silo_allowed_sites));

create policy "anon select" on silo_reference_images
  for select to anon, authenticated
  using (site_id in (select site_id from silo_allowed_sites));

create policy "anon update" on silo_reference_images
  for update to anon, authenticated
  using (site_id in (select site_id from silo_allowed_sites))
  with check (site_id in (select site_id from silo_allowed_sites));

create policy "anon delete" on silo_reference_images
  for delete to anon, authenticated
  using (site_id in (select site_id from silo_allowed_sites));

grant insert, select, update, delete on table public.silo_reference_images to anon, authenticated;

do $$
declare
  seq text;
begin
  seq := pg_get_serial_sequence('public.silo_reference_images', 'id');
  if seq is not null then
    execute format('grant usage, select on sequence %s to anon, authenticated', seq);
  end if;
end $$;

notify pgrst, 'reload schema';
