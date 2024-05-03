import os

from fixtures.neon_fixtures import NeonEnv, PgBin
from fixtures.utils import subprocess_capture


# Simple test to check that pg_waldump works with neon WAL files
def test_pg_waldump(neon_simple_env: NeonEnv, test_output_dir, pg_bin: PgBin):
    env = neon_simple_env
    env.neon_cli.create_branch("test_pg_waldump", "empty")
    endpoint = env.endpoints.create_start("test_pg_waldump")

    cur = endpoint.connect().cursor()
    cur.execute(
        """
        BEGIN;
        CREATE TABLE t1(i int primary key, n_updated int);
        INSERT INTO t1 select g, 0 from generate_series(1, 50) g;
        ROLLBACK;
    """
    )

    cur.execute(
        """
        BEGIN;
        CREATE TABLE t1(i int primary key, n_updated int);
        INSERT INTO t1 select g, 0 from generate_series(1, 50) g;
        COMMIT;
    """
    )

    # stop the endpoint to make sure that WAL files are flushed and won't change
    endpoint.stop()

    assert endpoint.pgdata_dir
    wal_path = os.path.join(endpoint.pgdata_dir, "pg_wal/000000010000000000000001")
    pg_waldump_path = os.path.join(pg_bin.pg_bin_path, "pg_waldump")

    # use special --ignore option to ignore the validation checks in pg_waldump
    # this is necessary, because neon WAL files contain gap at the beginning
    output_path, _, _ = subprocess_capture(test_output_dir, [pg_waldump_path, "--ignore", wal_path])

    with open(f"{output_path}.stdout", "r") as f:
        stdout = f.read()
        assert "ABORT" in stdout
        assert "COMMIT" in stdout