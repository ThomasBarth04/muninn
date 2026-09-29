#!/bin/sh
# First start only, as the postgres superuser (ADR 0003):
#   muninn_owner — owns the database and runs migrations. BYPASSRLS so the
#                  SECURITY DEFINER lookups in the migration can cross tenants.
#                  Not a superuser.
#   muninn_app   — what the app connects as. Owns nothing, bypasses nothing.
set -eu
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
  -v owner_pw="$MUNINN_OWNER_PASSWORD" -v app_pw="$MUNINN_APP_PASSWORD" <<'SQL'
CREATE ROLE muninn_owner LOGIN BYPASSRLS PASSWORD :'owner_pw';
CREATE ROLE muninn_app LOGIN PASSWORD :'app_pw';
CREATE DATABASE muninn OWNER muninn_owner;
SQL
