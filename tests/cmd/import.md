## dy import

```
$ dy import --help
Import items into a DynamoDB table from CSV/JSON file.

If you want to achieve best performance, recommendated way is to switch the table to OnDemand mode before import. (e.g. dy admin update table your_table --mode ondemand).

When you import items from a CSV file, header names are used to attributes for items. The first one or two column(s) would be primary key(s).

Usage: dy import [OPTIONS] --input-file <INPUT_FILE>

Options:
  -i, --input-file <INPUT_FILE>
          Filename contains DynamoDB items data. Specify appropriate format with --format option.

  -f, --format <FORMAT>
          Data format for import items.
          
            json = JSON format with newline/indent.
          
            jsonl = JSON Lines (http://jsonlines.org). i.e. one item per line.
          
            json-compact = JSON format, all items are packed in oneline.
          
            csv = comma-separated values with header. Header columns are considered to be DynamoDB attributes.
          
          [possible values: csv, json, jsonl, json-compact]

      --enable-set-inference
          Enable type inference for set types. This option is provided for backward compatibility

      --max-wcu <MAX_WCU>
          Maximum write capacity units (WCU) per second this import may consume.
          
          dynein starts from a realistic target derived from the table settings and probes upward, using this value as a hard ceiling.
          
          Set it to leave explicit headroom for production traffic on the same table.
          
          By default there is no ceiling: dynein's congestion control backs off on throttling and recovers on its own.

  -r, --region <REGION>
          The region to use (e.g. --region us-east-1). When using DynamodB Local, use `--region local`.
          You can use --region option in both top-level and subcommand-level.

  -p, --port <PORT>
          Specify the port number. This option has an effect only when `--region local` is used.

  -t, --table <TABLE>
          Target table of the operation. You can use --table option in both top-level and subcommand-level.
          You can store table schema locally by executing `$ dy use`, after that you need not to specify --table on every command.

  -h, --help
          Print help (see a summary with '-h')

$ dy help import
Import items into a DynamoDB table from CSV/JSON file.

If you want to achieve best performance, recommendated way is to switch the table to OnDemand mode before import. (e.g. dy admin update table your_table --mode ondemand).

When you import items from a CSV file, header names are used to attributes for items. The first one or two column(s) would be primary key(s).

Usage: dy import [OPTIONS] --input-file <INPUT_FILE>

Options:
  -i, --input-file <INPUT_FILE>
          Filename contains DynamoDB items data. Specify appropriate format with --format option.

  -f, --format <FORMAT>
          Data format for import items.
          
            json = JSON format with newline/indent.
          
            jsonl = JSON Lines (http://jsonlines.org). i.e. one item per line.
          
            json-compact = JSON format, all items are packed in oneline.
          
            csv = comma-separated values with header. Header columns are considered to be DynamoDB attributes.
          
          [possible values: csv, json, jsonl, json-compact]

      --enable-set-inference
          Enable type inference for set types. This option is provided for backward compatibility

      --max-wcu <MAX_WCU>
          Maximum write capacity units (WCU) per second this import may consume.
          
          dynein starts from a realistic target derived from the table settings and probes upward, using this value as a hard ceiling.
          
          Set it to leave explicit headroom for production traffic on the same table.
          
          By default there is no ceiling: dynein's congestion control backs off on throttling and recovers on its own.

  -r, --region <REGION>
          The region to use (e.g. --region us-east-1). When using DynamodB Local, use `--region local`.
          You can use --region option in both top-level and subcommand-level.

  -p, --port <PORT>
          Specify the port number. This option has an effect only when `--region local` is used.

  -t, --table <TABLE>
          Target table of the operation. You can use --table option in both top-level and subcommand-level.
          You can store table schema locally by executing `$ dy use`, after that you need not to specify --table on every command.

  -h, --help
          Print help (see a summary with '-h')

```
