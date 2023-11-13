#! /bin/bash

PG_URL=""
function pg.display_time()
{
	local T=$1
	local D=$((T/60/60/24))
	local H=$((T/60/60%24))
	local M=$((T/60%60))
	local S=$((T%60))
	printf '%dd:%02d:%02d:%02d\n' $D $H $M $S
}

function pg.get_last_pushed()
{
	curl --request "GET" --no-progress-meter "${PG_URL}/metrics" |
		grep "job=\"aperturec_" |
		grep ^push_time_seconds || return 1
}

function pg.get_acs_older_than()
{
	local max_age_secs="$1"
	local pattern="$2"

	(
		IFS=$'\n'
		for line in $(pg.get_last_pushed "${PG_URL}"); do
			local push_time; push_time=$(echo -e "${line}" | awk '{print $NF}')
			local secs_ago; secs_ago=$(awk 'BEGIN { print '"$(date +%s)"' - '"${push_time}"'}')
			secs_ago=${secs_ago%%.*}
			
			line="$(echo "${line}" | awk '{print $1}') : $(pg.display_time "${secs_ago}")"
			(( secs_ago >= max_age_secs )) &&
				echo "${line}" | grep "${pattern}"
		done |
			awk '{print $NF,$0}' |
			sort | cut -f2- -d' ' || return 1
	) || return 1
}

function pg.delete_acs_older_than()
{
	local max_age_secs="$1"
	local pattern="$2"

	(
		IFS=$'\n'
		for line in $(pg.get_acs_older_than "${max_age_secs}" "${pattern}"); do
			line=$(echo "${line}" |
				awk '{print $1}' |
				sed -e 's/^push_time_seconds{//' |
				sed 's/="/\//g' |
				sed 's/",/\//g' |
				sed 's/"}//')
			
			# Extract the job, it must be first in the URL
			local job; job=$(echo "$line" | sed -E "s/.*(\/job\/[^\/]*).*/\1/")
			line=${line//"${job}"}

			local url; url="${PG_URL}/metrics${job}/${line}"
			printf "DELETE %s\n" "${url}"
			curl --request "DELETE" --no-progress-meter "${url}" || break
		done || return 1
	) || return 1
}

function pg.clean_help()
{
	cat <<-\
	---------------------------------------------------------------------------
	Usage: pg_clean.sh [OPTIONS...]
	       source pg_clean.sh && pg.clean [OPTIONS...]

	This script can be used to clean the Prometheus Pushgateway in the event of
	stale metric data. Stale data can occur if an ApertureC process terminates
	unexpectedly or loses network connectivity before terminating.

	Recent versions of Pushgateway make it easy to delete stale metric groups
	from the exposed UI. Older versions of Pushgateway make cleaning up data
	much more difficult.

	With just a URL, pg.clean will display all metric groups with a "last
	pushed" time older than 5 minutes. The --age, --date, and --pattern options
	can be used individually or combined to select specific metric groups. Once
	the groups have been selected, re-run with the --clean option to remove
	them.

	 --url URL          Url and port of the pushgateway.
	                        eg http://127.0.0.1:9091
	                    URL can also be set via the PG_URL env var.
	 --age AGE          Only process metrics >= AGE in seconds. Default 300.
	                        Specify 0 to select all metrics.
	 --date STRING      Uses \`date --date\` command to calculate age from a
	                        human readable string. eg "3 days ago"
	 --pattern PATTERN  Only process metrics that match this \`grep\` pattern
	 --clean            Deletes the selected metrics from the pushgateway

	---------------------------------------------------------------------------
}

function pg.clean()
{
	local AGE_DEFAULT=$((60*5));
	local age=${AGE_DEFAULT};
	local do_get=1;
	local do_delete=0;
	local pattern="job";

	while [[ $# -gt 0 ]]; do
		case "$1" in
			-u|--url)
				PG_URL="$2"
				shift 2;;
			-a|--age)
				age="$2"
				shift 2;;
			--clean)
				do_delete=1
				do_get=0
				shift;;
			--date)
				date --date="$2" 1> /dev/null || return 1
				age=$(($(date +%s) - $(date --date="$2" +%s)))
				shift 2;;
			-p|--pattern)
				pattern="$2"
				shift 2;;
			*)
				pg.clean_help
				return
				shift;;
		esac
	done

	[[ ${age} -ne ${AGE_DEFAULT} ]] &&
		printf "max_age: %ds\n" "${age}";

	if [[ -z "${PG_URL}" ]]; then
		printf 'No pushgateway URL supplied. Either set PG_URL in your env or specify --url\n'
		return
	fi

	if [[ ${do_get} -eq 1 ]]; then
		pg.get_acs_older_than "${age}" "${pattern}" || return 1
	fi

	if [[ ${do_delete} -eq 1 ]]; then
		pg.delete_acs_older_than "${age}" "${pattern}" || return 1
	fi
}

function __pg_clean_complete()
{
	local current=${COMP_WORDS[COMP_CWORD]}
	local opts="--url --age --clean --date --pattern --help"
	mapfile -t COMPREPLY < <(compgen -W "${opts}" -- "${current}")
}
complete -F __pg_clean_complete pg.clean

if [[ "$0" == "${BASH_SOURCE[0]}" ]]; then
	set -eu
	pg.clean "$@"
fi
