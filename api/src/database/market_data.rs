use crate::Result;
use async_trait::async_trait;
use mbn::enums::{RType, Schema};
use mbn::record_enum::RecordEnum;
use mbn::records::{BboMsg, BidAskPair, Mbp1Msg, OhlcvMsg, RecordHeader, TbboMsg, TradeMsg};
use mbn::symbols::SymbolMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::os::raw::c_char;
use std::str::FromStr;

// Function to compute a unique hash for the order book state
fn compute_order_book_hash(levels: &[BidAskPair]) -> String {
    let mut hasher = Sha256::new();

    for level in levels {
        hasher.update(level.bid_px.to_be_bytes());
        hasher.update(level.ask_px.to_be_bytes());
        hasher.update(level.bid_sz.to_be_bytes());
        hasher.update(level.ask_sz.to_be_bytes());
        hasher.update(level.bid_ct.to_be_bytes());
        hasher.update(level.ask_ct.to_be_bytes());
    }

    let result = hasher.finalize();

    // Convert hash result to a hex string
    result.iter().map(|byte| format!("{:02x}", byte)).collect()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RetrieveParams {
    pub symbols: Vec<String>,
    pub start_ts: i64,
    pub end_ts: i64,
    pub schema: String,
}

impl RetrieveParams {
    fn schema(&self) -> Result<Schema> {
        let schema = Schema::from_str(&self.schema)?;
        Ok(schema)
    }

    fn schema_interval(&self) -> Result<i64> {
        let schema = Schema::from_str(&self.schema)?;

        match schema {
            Schema::Mbp1 => Ok(1), // 1 nanosecond
            Schema::Trade => Ok(1),
            Schema::Tbbo => Ok(1),
            Schema::Ohlcv1S => Ok(1_000_000_000), // 1 second in nanoseconds
            Schema::Ohlcv1M => Ok(60_000_000_000), // 1 minute in nanoseconds
            Schema::Ohlcv1H => Ok(3_600_000_000_000), // 1 hour in nanoseconds
            Schema::Ohlcv1D => Ok(86_400_000_000_000), // 1 day in nanoseconds
            Schema::Bbo1S => Ok(1_000_000_000),
            Schema::Bbo1M => Ok(60_000_000_000),
        }
    }

    fn interval_adjust_ts(&self, ts: i64, interval_ns: i64) -> Result<i64> {
        Ok(ts - (ts % interval_ns))
    }

    fn batch_interval(&mut self, interval_ns: i64, batch_size: i64) -> Result<i64> {
        // Adjust start timestamp
        self.start_ts = self.interval_adjust_ts(self.start_ts, interval_ns)?;
        let calculated_end_ts = self.start_ts + batch_size;

        // If the calculated end_ts exceeds the emarketed end_ts, use the requested end_ts
        if calculated_end_ts > self.end_ts {
            let end = self.interval_adjust_ts(self.end_ts, interval_ns)?;
            Ok(end)
            //Ok(self.end_ts)// Ensure we do not go beyond the true end_ts
        } else {
            Ok(calculated_end_ts)
        }
    }
    fn rtype(&self) -> Result<RType> {
        let schema = Schema::from_str(&self.schema)?;
        Ok(RType::from(schema))
    }
}

// impl RetrieveParams {}

#[async_trait]
pub trait RecordInsertQueries {
    async fn insert_query(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()>;
}

#[async_trait]
impl RecordInsertQueries for Mbp1Msg {
    async fn insert_query(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        // Compute the order book hash based on levels
        let order_book_hash = compute_order_book_hash(&self.levels);

        // Insert into mbp table
        let mbp_id: i32 = sqlx::query_scalar(
            r#"
            INSERT INTO mbp (instrument_id, ts_event, price, size, action, side,flags, ts_recv, ts_in_delta, sequence, order_book_hash)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            RETURNING id
            "#
        )
        .bind(self.hd.instrument_id as i32)
        .bind(self.hd.ts_event as i64)
        .bind(self.price)
        .bind(self.size as i32)
        .bind(self.action as i32)
        .bind(self.side as i32)
        .bind(self.flags as i32)
        .bind(self.ts_recv as i64)
        .bind(self.ts_in_delta)
        .bind(self.sequence as i32)
        .bind(&order_book_hash) // Bind the computed hash
        .fetch_one(&mut *tx)
        .await?;

        // Insert into bid_ask table
        for (depth, level) in self.levels.iter().enumerate() {
            let _ = sqlx::query(
                r#"
                INSERT INTO bid_ask (mbp_id, depth, bid_px, bid_sz, bid_ct, ask_px, ask_sz, ask_ct)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                "#,
            )
            .bind(mbp_id)
            .bind(depth as i32) // Using the index as depth
            .bind(level.bid_px)
            .bind(level.bid_sz as i32)
            .bind(level.bid_ct as i32)
            .bind(level.ask_px)
            .bind(level.ask_sz as i32)
            .bind(level.ask_ct as i32)
            .execute(&mut *tx)
            .await?;
        }

        Ok(())
    }
}

pub async fn insert_records(
    tx: &mut Transaction<'_, Postgres>,
    records: Vec<Mbp1Msg>,
) -> Result<()> {
    for record in records {
        record.insert_query(tx).await?;
    }

    Ok(())
}

#[async_trait]
pub trait RecordRetrieveQueries: Sized {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)>;
}

pub trait FromRow: Sized {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self>;
}

#[async_trait]
impl RecordRetrieveQueries for Mbp1Msg {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)> {
        // Batch timestamps
        let interval_ns = params.schema_interval()?;
        let end_ts = params.batch_interval(interval_ns, batch)?;

        // Convert the Vec<String> symbols to an array for binding
        let symbol_array: Vec<&str> = params.symbols.iter().map(AsRef::as_ref).collect();

        // Construct the SQL query with a join and additional filtering by symbols
        let mut query = r#"
            SELECT m.instrument_id, m.ts_event, m.price, m.size, m.action, m.side, m.flags, m.ts_recv, m.ts_in_delta, m.sequence, i.ticker,
                   b.bid_px, b.bid_sz, b.bid_ct, b.ask_px, b.ask_sz, b.ask_ct
            FROM mbp m
            INNER JOIN instrument i ON m.instrument_id = i.id
            LEFT JOIN bid_ask b ON m.id = b.mbp_id AND b.depth = 0
            WHERE m.ts_recv BETWEEN $1 AND $2
            AND i.ticker = ANY($3)
        "#.to_string();
        // ORDER / /`` BY m.ts_event

        // Dynamically add the action filter for Tbbo schema
        if params.schema()? == Schema::Tbbo {
            query.push_str(" AND m.action = 84"); // Action 'T' (ASCII 84) for trade
        }

        // Add the ORDER BY clause
        // query.push_str(" ORDER BY m.ts_event");

        // Execute the query with parameters, including LIMIT and OFFSET
        let rows = sqlx::query(&query)
            .bind(params.start_ts)
            .bind(end_ts)
            .bind(&symbol_array)
            .fetch_all(pool)
            .await?;

        // Update start
        params.start_ts = end_ts + 1;

        let mut records = Vec::new();
        let mut symbol_map = SymbolMap::new();

        for row in rows {
            let instrument_id = row.try_get::<i32, _>("instrument_id")? as u32;
            let ticker: String = row.try_get("ticker")?;
            let record = Mbp1Msg::from_row(row)?;

            records.push(record);
            symbol_map.add_instrument(&ticker, instrument_id);
        }

        Ok((records, symbol_map))
    }
}

impl FromRow for Mbp1Msg {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Mbp1Msg {
            hd: RecordHeader::new::<Mbp1Msg>(
                row.try_get::<i32, _>("instrument_id")? as u32,
                row.try_get::<i64, _>("ts_event")? as u64,
            ),
            price: row.try_get::<i64, _>("price")?,
            size: row.try_get::<i32, _>("size")? as u32,
            action: row.try_get::<i32, _>("action")? as c_char,
            side: row.try_get::<i32, _>("side")? as c_char,
            flags: row.try_get::<i32, _>("flags")? as u8,
            depth: 0 as u8, // Always top of book

            // flags: row.try_get::<i32, _>("flags")? as u8,
            ts_recv: row.try_get::<i64, _>("ts_recv")? as u64,
            ts_in_delta: row.try_get::<i32, _>("ts_in_delta")?,
            sequence: row.try_get::<i32, _>("sequence")? as u32,
            levels: [BidAskPair {
                bid_px: row.try_get::<i64, _>("bid_px")?,
                ask_px: row.try_get::<i64, _>("ask_px")?,
                bid_sz: row.try_get::<i32, _>("bid_sz")? as u32,
                ask_sz: row.try_get::<i32, _>("ask_sz")? as u32,
                bid_ct: row.try_get::<i32, _>("bid_ct")? as u32,
                ask_ct: row.try_get::<i32, _>("ask_ct")? as u32,
            }],
        })
    }
}

#[async_trait]
impl RecordRetrieveQueries for TradeMsg {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)> {
        // Batch timestamps
        let interval_ns = params.schema_interval()?;
        let end_ts = params.batch_interval(interval_ns, batch)?;

        // Convert the Vec<String> symbols to an array for binding
        let symbol_array: Vec<&str> = params.symbols.iter().map(AsRef::as_ref).collect();

        // Construct the SQL query with a join and additional filtering by symbols
        let query = r#"
            SELECT m.instrument_id, m.ts_event, m.price, m.size, m.action, m.side, m.flags, m.ts_recv, m.ts_in_delta, m.sequence, i.ticker
            FROM mbp m
            INNER JOIN instrument i ON m.instrument_id = i.id
            LEFT JOIN bid_ask b ON m.id = b.mbp_id AND b.depth = 0
            WHERE m.ts_recv BETWEEN $1 AND $2
            AND i.ticker = ANY($3)
            AND m.action = 84  -- Filter only trades where action is 'T' (ASCII 84)
            ORDER BY m.ts_event
        "#;

        // Execute the query with parameters, including LIMIT and OFFSET
        let rows = sqlx::query(query)
            .bind(params.start_ts)
            .bind(end_ts)
            .bind(&symbol_array)
            .fetch_all(pool)
            .await?;

        // Update start
        params.start_ts = end_ts + 1;

        let mut records = Vec::new();
        let mut symbol_map = SymbolMap::new();

        for row in rows {
            let instrument_id = row.try_get::<i32, _>("instrument_id")? as u32;
            let ticker: String = row.try_get("ticker")?;
            let record = TradeMsg::from_row(row)?;

            records.push(record);
            symbol_map.add_instrument(&ticker, instrument_id);
        }

        Ok((records, symbol_map))
    }
}

impl FromRow for TradeMsg {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(TradeMsg {
            hd: RecordHeader::new::<TradeMsg>(
                row.try_get::<i32, _>("instrument_id")? as u32,
                row.try_get::<i64, _>("ts_event")? as u64,
            ),
            price: row.try_get::<i64, _>("price")?,
            size: row.try_get::<i32, _>("size")? as u32,
            action: row.try_get::<i32, _>("action")? as c_char,
            side: row.try_get::<i32, _>("side")? as c_char,
            flags: row.try_get::<i32, _>("flags")? as u8,
            depth: 0 as u8, // Always top of book
            ts_recv: row.try_get::<i64, _>("ts_recv")? as u64,
            ts_in_delta: row.try_get::<i32, _>("ts_in_delta")?,
            sequence: row.try_get::<i32, _>("sequence")? as u32,
        })
    }
}

#[async_trait]
impl RecordRetrieveQueries for BboMsg {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)> {
        // Batch timestamps
        let interval_ns = params.schema_interval()?;
        let end_ts = params.batch_interval(interval_ns, batch)?;

        // Convert the Vec<String> symbols to an array for binding
        let symbol_array: Vec<&str> = params.symbols.iter().map(AsRef::as_ref).collect();

        // Construct the SQL query with a join and additional filtering by symbols
        let query = r#"
        WITH ordered_data AS (
            SELECT
                m.id,
                m.instrument_id,
                m.ts_event,
                m.price,
                m.size,
                m.action,
                m.side,
                m.flags,
                m.sequence,
                m.ts_recv,
                b.bid_px,
                b.ask_px,
                b.bid_sz,
                b.ask_sz,
                b.bid_ct,
                b.ask_ct,
                row_number() OVER (PARTITION BY m.instrument_id, floor((m.ts_recv - 1) / $3) * $3 ORDER BY m.ts_recv ASC, m.ctid ASC) AS first_row,
                row_number() OVER (PARTITION BY m.instrument_id, floor((m.ts_recv - 1) / $3) * $3 ORDER BY m.ts_recv DESC, m.ctid DESC) AS last_row
            FROM mbp m
            INNER JOIN instrument i ON m.instrument_id = i.id
            LEFT JOIN bid_ask b ON m.id = b.mbp_id AND b.depth = 0
            WHERE m.ts_recv BETWEEN $1 AND $2
            AND i.ticker = ANY($4)
        ),
        -- Subquery to get the last trade event
        trade_data AS (
            SELECT
                instrument_id,
                floor((ts_recv - 1) / $3) * $3 AS ts_recv_start,
                MAX(ts_recv) AS last_trade_ts_recv,
                 --MAX(ts_event) AS last_trade_ts_event,
                MAX(id) AS last_trade_id  
            FROM ordered_data
            WHERE action = 84 -- Only consider trades (action = 84)
            GROUP BY instrument_id, floor((ts_recv - 1) / $3) * $3
        ),
        aggregated_data AS (
            SELECT
                o.instrument_id,
                floor((o.ts_recv - 1) / $3) * $3 AS ts_recv,
                MAX(o.ts_event) FILTER (WHERE o.ts_recv = t.last_trade_ts_recv AND o.id = t.last_trade_id AND o.action = 84) AS ts_event,  -- Correct reference for ts_event
                MIN(o.bid_px) FILTER (WHERE o.last_row = 1) AS last_bid_px,
                MIN(o.ask_px) FILTER (WHERE o.last_row = 1) AS last_ask_px,
                MIN(o.bid_sz) FILTER (WHERE o.last_row = 1) AS last_bid_sz,
                MIN(o.ask_sz) FILTER (WHERE o.last_row = 1) AS last_ask_sz,
                MIN(o.bid_ct) FILTER (WHERE o.last_row = 1) AS last_bid_ct,
                MIN(o.ask_ct) FILTER (WHERE o.last_row = 1) AS last_ask_ct,
                MAX(o.price) FILTER (WHERE o.ts_recv = t.last_trade_ts_recv AND o.id = t.last_trade_id AND o.action = 84) AS last_trade_price,
                MAX(o.size) FILTER (WHERE o.ts_recv = t.last_trade_ts_recv AND o.id = t.last_trade_id AND o.action = 84) AS last_trade_size,
                MAX(o.side) FILTER (WHERE o.ts_recv = t.last_trade_ts_recv AND o.id = t.last_trade_id AND o.action = 84) AS last_trade_side,
                MAX(o.flags) FILTER (WHERE o.last_row = 1) AS last_trade_flags,
                MIN(o.sequence) FILTER (WHERE o.last_row = 1) AS last_trade_sequence
            FROM ordered_data o
            LEFT JOIN trade_data t ON o.instrument_id = t.instrument_id AND floor((o.ts_recv - 1) / $3) * $3 = t.ts_recv_start
            GROUP BY o.instrument_id, floor((o.ts_recv - 1) / $3) * $3, t.last_trade_ts_recv, t.last_trade_id
        ),
        -- Step 1: Forward-fill ts_event
        filled_ts_event AS (
            SELECT
                a.instrument_id,
                MAX(a.ts_event) OVER (PARTITION BY a.instrument_id ORDER BY a.ts_recv ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS ts_event,  -- Forward-fill ts_event
                a.ts_recv,
                a.last_bid_px,
                a.last_ask_px,
                a.last_bid_sz,
                a.last_ask_sz,
                a.last_bid_ct,
                a.last_ask_ct,
                a.last_trade_price,
                a.last_trade_size,
                a.last_trade_side,
                a.last_trade_flags,
                a.last_trade_sequence
            FROM aggregated_data a
        ),
        -- Step 2: Forward-fill price and size based on the now-filled ts_event
        filled_price_size AS (
            SELECT
                f.instrument_id,
                f.ts_event,
                f.ts_recv,
                find_last_ignore_nulls(f.last_trade_price) OVER (PARTITION BY f.instrument_id ORDER BY f.ts_recv) AS price,
                find_last_ignore_nulls(f.last_trade_size) OVER (PARTITION BY f.instrument_id ORDER BY f.ts_recv) AS size,
                find_last_ignore_nulls(f.last_trade_side) OVER (PARTITION BY f.instrument_id ORDER BY f.ts_recv) AS side,
                f.last_bid_px AS bid_px,
                f.last_ask_px AS ask_px,
                f.last_bid_sz AS bid_sz,
                f.last_ask_sz AS ask_sz,
                f.last_bid_ct AS bid_ct,
                f.last_ask_ct AS ask_ct,
                f.last_trade_flags AS flags,
                f.last_trade_sequence AS sequence
            FROM filled_ts_event f
        )
        SELECT
            fp.instrument_id,
            fp.ts_event,
            CAST(fp.ts_recv + $3 AS BIGINT) AS ts_recv, -- The floored ts_recv used for grouping
            fp.bid_px,
            fp.ask_px,
            fp.bid_sz,
            fp.ask_sz,
            fp.bid_ct,
            fp.ask_ct,
            fp.price,  -- Forward-filled price based on ts_event
            fp.size,   -- Forward-filled size based on ts_event
            fp.side,
            fp.flags,
            fp.sequence,
            i.ticker
        FROM filled_price_size fp
        INNER JOIN instrument i ON fp.instrument_id = i.id
        ORDER BY fp.ts_recv;
        "#;
        let rows = sqlx::query(query)
            .bind(params.start_ts as i64)
            .bind(end_ts as i64)
            .bind(interval_ns as i64)
            .bind(&symbol_array)
            .fetch_all(pool)
            .await?;

        // Update start
        params.start_ts = end_ts; // + 1;

        let mut records = Vec::new();
        let mut symbol_map = SymbolMap::new();

        for row in rows {
            let instrument_id = row.try_get::<i32, _>("instrument_id")? as u32;
            let ticker: String = row.try_get("ticker")?;
            let record = BboMsg::from_row(row)?;

            records.push(record);
            symbol_map.add_instrument(&ticker, instrument_id);
        }

        Ok((records, symbol_map))
    }
}

impl FromRow for BboMsg {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(BboMsg {
            hd: RecordHeader::new::<BboMsg>(
                row.try_get::<i32, _>("instrument_id")? as u32,
                row.try_get::<i64, _>("ts_event").unwrap_or(0) as u64,
            ),
            price: row.try_get::<i64, _>("price").unwrap_or(0),
            size: row.try_get::<i32, _>("size").unwrap_or(0) as u32,
            side: row.try_get::<i32, _>("side").unwrap_or(78) as c_char,
            flags: row.try_get::<i32, _>("flags")? as u8,
            ts_recv: row.try_get::<i64, _>("ts_recv")? as u64,
            sequence: row.try_get::<i32, _>("sequence")? as u32,
            levels: [BidAskPair {
                bid_px: row.try_get::<i64, _>("bid_px")?,
                ask_px: row.try_get::<i64, _>("ask_px")?,
                bid_sz: row.try_get::<i32, _>("bid_sz")? as u32,
                ask_sz: row.try_get::<i32, _>("ask_sz")? as u32,
                bid_ct: row.try_get::<i32, _>("bid_ct")? as u32,
                ask_ct: row.try_get::<i32, _>("ask_ct")? as u32,
            }],
        })
    }
}

#[async_trait]
impl RecordRetrieveQueries for OhlcvMsg {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)> {
        // Batch timestamps
        let interval_ns = params.schema_interval()?;
        let end_ts = params.batch_interval(interval_ns, batch)?;

        // Convert the Vec<String> symbols to an array for binding
        let symbol_array: Vec<&str> = params.symbols.iter().map(AsRef::as_ref).collect();

        let rows = sqlx::query(
        r#"
        WITH ordered_data AS (
          SELECT
            m.instrument_id,
            m.ts_recv,
            m.price,
            m.size,
            row_number() OVER (PARTITION BY m.instrument_id, floor(m.ts_recv / $3) * $3 ORDER BY m.ts_recv ASC,  m.ctid ASC) AS first_row,
            row_number() OVER (PARTITION BY m.instrument_id, floor(m.ts_recv / $3) * $3 ORDER BY m.ts_recv DESC, m.ctid DESC) AS last_row
          FROM mbp m
          INNER JOIN instrument i ON m.instrument_id = i.id
          WHERE m.ts_recv BETWEEN $1 AND $2
          AND i.ticker = ANY($4)
          AND m.action = 84  -- Filter only trades where action is 'T' (ASCII 84)
        ),
        aggregated_data AS (
          SELECT
            instrument_id,
            floor(ts_recv / $3) * $3 AS ts_event, -- Maintain nanoseconds
            MIN(price) FILTER (WHERE first_row = 1) AS open,
            MIN(price) FILTER (WHERE last_row = 1) AS close,
            MIN(price) AS low,
            MAX(price) AS high,
            SUM(size) AS volume
          FROM ordered_data
          GROUP BY
            instrument_id,
            floor(ts_recv / $3) * $3
        )
        SELECT
          a.instrument_id,
          CAST(a.ts_event AS BIGINT), -- Keep as nanoseconds
          a.open,
          a.close,
          a.low,
          a.high,
          a.volume,
          i.ticker
        FROM aggregated_data a
        INNER JOIN instrument i ON a.instrument_id = i.id
        ORDER BY a.ts_event
        "#
        )
        .bind(params.start_ts as i64)
        .bind(end_ts - 1)
        .bind(interval_ns)
        .bind(&symbol_array)
        .fetch_all(pool)
        .await?;

        // Update start
        params.start_ts = end_ts;

        let mut records = Vec::new();
        let mut symbol_map = SymbolMap::new();

        for row in rows {
            let instrument_id = row.try_get::<i32, _>("instrument_id")? as u32;
            let ticker: String = row.try_get("ticker")?;
            let record = OhlcvMsg::from_row(row)?;

            records.push(record);
            symbol_map.add_instrument(&ticker, instrument_id);
        }

        Ok((records, symbol_map))
    }
}

impl FromRow for OhlcvMsg {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(OhlcvMsg {
            hd: RecordHeader::new::<OhlcvMsg>(
                row.try_get::<i32, _>("instrument_id")? as u32,
                row.try_get::<i64, _>("ts_event")? as u64,
            ),
            open: row.try_get::<i64, _>("open")?,
            close: row.try_get::<i64, _>("close")?,
            low: row.try_get::<i64, _>("low")?,
            high: row.try_get::<i64, _>("high")?,
            volume: row.try_get::<i64, _>("volume")? as u64,
        })
    }
}

#[async_trait]
impl RecordRetrieveQueries for RecordEnum {
    async fn retrieve_query(
        pool: &PgPool,
        params: &mut RetrieveParams,
        batch: i64,
    ) -> Result<(Vec<Self>, SymbolMap)> {
        let (records, map) = match RType::from(params.rtype().unwrap()) {
            RType::Mbp1 => {
                let (records, map) = Mbp1Msg::retrieve_query(pool, params, batch).await?;
                (records.into_iter().map(RecordEnum::Mbp1).collect(), map)
            }
            RType::Ohlcv => {
                let (records, map) = OhlcvMsg::retrieve_query(pool, params, batch).await?;
                (records.into_iter().map(RecordEnum::Ohlcv).collect(), map)
            }
            RType::Trade => {
                let (records, map) = TradeMsg::retrieve_query(pool, params, batch).await?;
                (records.into_iter().map(RecordEnum::Trade).collect(), map)
            }
            RType::Bbo => {
                let (records, map) = BboMsg::retrieve_query(pool, params, batch).await?;
                (records.into_iter().map(RecordEnum::Bbo).collect(), map)
            }
            RType::Tbbo => {
                let (records, map) = TbboMsg::retrieve_query(pool, params, batch).await?;
                (records.into_iter().map(RecordEnum::Tbbo).collect(), map)
            }
        };
        Ok((records, map))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::database::init::init_market_db;
    use crate::database::symbols::*;
    // use mbn::enums::Schema;
    use mbn::enums::{Action, Side};
    use mbn::symbols::Instrument;
    use serial_test::serial;

    async fn create_instrument(pool: &PgPool) -> Result<i32> {
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        let ticker = "AAPL";
        let name = "Apple Inc.";
        let instrument = Instrument::new(ticker, name, None);
        let id = instrument
            .insert_instrument(&mut transaction)
            .await
            .expect("Error inserting symbol.");
        let _ = transaction.commit().await?;
        Ok(id)
    }

    #[test]
    fn test_retrieve_params_schema() -> anyhow::Result<()> {
        let params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("mbp-1"),
        };

        // Test
        assert_eq!(params.schema()?, Schema::Mbp1);

        Ok(())
    }
    #[test]
    fn test_retrieve_params_schema_interval() -> anyhow::Result<()> {
        let params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("tbbo"),
        };

        // Test
        assert_eq!(params.schema_interval()?, 1);

        Ok(())
    }

    #[test]
    fn test_retrieve_params_rtype() -> anyhow::Result<()> {
        let params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1728878401000000000,
            end_ts: 1728878460000000000,
            schema: String::from("ohlcv-1h"),
        };

        // Test
        assert_eq!(params.rtype()?, RType::Ohlcv);

        Ok(())
    }

    #[test]
    fn test_retrieve_batch_interval() -> anyhow::Result<()> {
        let batch_size: i64 = 86_400_000_000_000;
        let interval_ns: i64 = 86_400_000_000_000;

        let mut params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1728878401000000000, //2024-10-14 04:00:01 UTC
            end_ts: 1728878460000000000,   //2024-10-14 04:01:00 UTC
            schema: String::from("ohlcv-1d"),
        };

        // Test
        let end = params.batch_interval(interval_ns, batch_size)?;

        // Validate
        //(same because the user requested a end_ts thats close to start _ts than one interval fo the schema)
        assert_eq!(params.start_ts, 1728864000000000000); //2024-10-14 00:00 UTC
        assert_eq!(end, 1728864000000000000); //2024-10-14 00:00 UTC

        Ok(())
    }

    #[test]
    fn test_retrieve_batch_interval_end_gt_batch() -> anyhow::Result<()> {
        let batch_size: i64 = 86_400_000_000_000;
        let interval_ns: i64 = 86_400_000_000_000;

        let mut params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1728878401000000000, //2024-10-14 04:00:01 UTC
            end_ts: 1729396801000000000,   //2024-10-20 04:00:01 UTC
            schema: String::from("ohlcv-1d"),
        };

        // Test
        let end = params.batch_interval(interval_ns, batch_size)?;

        // Validate
        //(same because the user requested a end_ts thats close to start _ts than one interval fo the schema)
        assert_eq!(params.start_ts, 1728864000000000000); //2024-10-14 00:00 UTC
        assert_eq!(end, 1728950400000000000); //2024-10-15 00:00 UTC

        Ok(())
    }

    #[test]
    fn test_retrieve_batch_interval_end_lt_batch() -> anyhow::Result<()> {
        let batch_size: i64 = 86_400_000_000_000 * 7;
        let interval_ns: i64 = 86_400_000_000_000;

        let mut params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1728878401000000000, //2024-10-14 04:00:01 UTC
            end_ts: 1729396801000000000,   //2024-10-20 04:00:01 UTC
            schema: String::from("ohlcv-1d"),
        };

        // Test
        let end = params.batch_interval(interval_ns, batch_size)?;

        // Validate
        //(same because the user requested a end_ts thats close to start _ts than one interval fo the schema)
        assert_eq!(params.start_ts, 1728864000000000000); //2024-10-14 00:00 UTC
        assert_eq!(end, 1729382400000000000); //2024-10-20 00:00 UTC

        Ok(())
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_create_record() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Add as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704295503644092562) },
                price: 6870,
                size: 2,
                action: Action::Add as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        // Test
        let result = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");

        // Validate
        assert_eq!(result, ());

        // Cleanup
        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_retrieve_mbp1() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Add as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092565) },
                price: 6870,
                size: 2,
                action: Action::Add as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092565,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        let _ = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");
        let _ = transaction.commit().await;

        // Test
        let mut query_params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("mbp-1"),
        };

        let (records, _hash_map) =
            Mbp1Msg::retrieve_query(&pool, &mut query_params, 86400000000000)
                .await
                .expect("Error on retrieve records.");

        // Validate
        assert!(records.len() > 0);

        // Cleanup
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_retrieve_tbbo() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092565) },
                price: 6870,
                size: 2,
                action: Action::Add as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092565,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        let _ = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");
        let _ = transaction.commit().await;

        // Test
        let mut query_params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("tbbo"),
        };

        let (records, _hash_map) =
            TbboMsg::retrieve_query(&pool, &mut query_params, 86400000000000)
                .await
                .expect("Error on retrieve records.");

        // Validate
        assert!(records.len() == 1);

        // Cleanup
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_retrieve_trade() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092565) },
                price: 6870,
                size: 2,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092565,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        let _ = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");
        let _ = transaction.commit().await;

        // Test
        let mut query_params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("trade"),
        };

        let (records, _hash_map) =
            TradeMsg::retrieve_query(&pool, &mut query_params, 86400000000000)
                .await
                .expect("Error on retrieve records.");

        // Validate
        assert!(records.len() == 2);

        // Cleanup
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_retrieve_bbo() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092565) },
                price: 6870,
                size: 2,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092565,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        let _ = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");
        let _ = transaction.commit().await;

        // Test
        let mut query_params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092563,
            end_ts: 1704209903644092567,
            schema: String::from("bbo-1s"),
        };

        let (records, _hash_map) = BboMsg::retrieve_query(&pool, &mut query_params, 86400000000000)
            .await
            .expect("Error on retrieve records.");

        // Validate
        assert!(records.len() > 0);

        // Cleanup
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }

    #[sqlx::test]
    #[serial]
    // #[ignore]
    async fn test_retrieve_ohlcv() {
        dotenv::dotenv().ok();
        let pool = init_market_db().await.unwrap();

        let instrument_id = create_instrument(&pool)
            .await
            .expect("Error creating instrument.");

        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        // Mock data
        let records = vec![
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092562) },
                price: 500,
                size: 1,
                action: Action::Trade as c_char,
                side: Side::Bid as c_char,
                depth: 0,
                flags: 0,
                ts_recv: 1704209103644092562,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704209103644092564) },
                price: 6770,
                size: 1,
                action: Action::Trade as c_char,
                side: 2,
                depth: 0,
                flags: 0,
                ts_recv: 1704209104645092564,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
            Mbp1Msg {
                hd: { RecordHeader::new::<Mbp1Msg>(instrument_id as u32, 1704295503644092562) },
                price: 6870,
                size: 2,
                action: Action::Trade as c_char,
                side: 2,
                depth: 0,
                flags: 0,
                ts_recv: 1704295503644092562,
                ts_in_delta: 17493,
                sequence: 739763,
                levels: [BidAskPair {
                    bid_px: 1,
                    ask_px: 1,
                    bid_sz: 1,
                    ask_sz: 1,
                    bid_ct: 10,
                    ask_ct: 20,
                }],
            },
        ];

        let _ = insert_records(&mut transaction, records)
            .await
            .expect("Error inserting records.");
        let _ = transaction.commit().await;

        // Test
        let mut query_params = RetrieveParams {
            symbols: vec!["AAPL".to_string()],
            start_ts: 1704209103644092562,
            end_ts: 1704295503654092563,
            schema: String::from("ohlcv-1d"),
        };

        let (result, _hash_map) =
            OhlcvMsg::retrieve_query(&pool, &mut query_params, 86_400_000_000_000)
                .await
                .expect("Error on retrieve records.");

        // Validate
        assert!(result.len() > 0);

        // Cleanup
        let mut transaction = pool
            .begin()
            .await
            .expect("Error setting up test transaction.");

        Instrument::delete_instrument(&mut transaction, instrument_id)
            .await
            .expect("Error on delete.");

        let _ = transaction.commit().await;
    }
}