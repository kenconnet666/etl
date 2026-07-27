#!/usr/bin/env bash
set -uo pipefail
export PATH=/usr/local/bin:/usr/bin:/bin
echo "cores: $(nproc), ram: $(free -g | awk '/Mem:/{print $2}') GB"
duckdb -c "
select name, value
from duckdb_settings()
where name in ('memory_limit', 'threads', 'temp_directory',
               'max_temp_directory_size', 'preserve_insertion_order',
               'allocator_background_threads')
order by name;
"
