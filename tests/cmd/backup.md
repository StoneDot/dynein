## dy backup

```
$ dy backup --help
dy-backup 0.2.1
Take backup of a DynamoDB table using on-demand backup

For more details: https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/BackupRestore.html

USAGE:
    dy backup [FLAGS] [OPTIONS]

FLAGS:
        --all-tables
            List backups for all tables in the region

    -h, --help
            Prints help information

    -l, --list
            List existing DynamoDB backups

    -V, --version
            Prints version information


OPTIONS:
        --endpoint-url <endpoint-url>
            Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-2.amazonaws.com/). If you use this
            option with `--port`, the endpoint is rewritten by the value of`--port`. Stored config of port does not
            affect the specified endpoint. Please note that the endpoint's URL and the specified region should align
    -p, --port <port>
            Specify the port number. This option has an effect only when `--region local` is used

    -r, --region <region>
            The region to use (e.g. --region us-east-1). When using DynamodB Local, use `--region local`. You can use
            --region option in both top-level and subcommand-level
    -t, --table <table>
            Target table of the operation. You can use --table option in both top-level and subcommand-level. You can
            store table schema locally by executing `$ dy use`, after that you need not to specify --table on every
            command

$ dy help backup
dy-backup 0.2.1
Take backup of a DynamoDB table using on-demand backup

For more details: https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/BackupRestore.html

USAGE:
    dy backup [FLAGS] [OPTIONS]

FLAGS:
        --all-tables    List backups for all tables in the region
    -h, --help          Prints help information
    -l, --list          List existing DynamoDB backups
    -V, --version       Prints version information

OPTIONS:
        --endpoint-url <endpoint-url>    Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-
                                         2.amazonaws.com/). If you use this option with
                                         `--port`, the endpoint is rewritten by the value of`--port`. Stored config of
                                         port does not affect the specified endpoint. Please note that the endpoint's
                                         URL and the specified region should align
    -p, --port <port>                    Specify the port number. This option has an effect only when `--region local`
                                         is used
    -r, --region <region>                The region to use (e.g. --region us-east-1). When using DynamodB Local, use
                                         `--region local`. You can use --region option in both top-level and subcommand-
                                         level
    -t, --table <table>                  Target table of the operation. You can use --table option in both top-level and
                                         subcommand-level. You can store table schema locally by executing `$ dy use`,
                                         after that you need not to specify --table on every command

```
