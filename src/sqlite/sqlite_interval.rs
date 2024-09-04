use datafusion::error::DataFusionError;
use datafusion::sql::sqlparser::ast::{
    self, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArgumentList, Ident,
    VisitorMut,
};
use std::fmt::Display;
use std::ops::ControlFlow;
use std::str::FromStr;

#[derive(Default)]
pub struct SQLiteVisitor {}

#[derive(Default, Debug)]
struct IntervalParts {
    years: i64,
    months: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    nanos: u32,
}

enum SQLiteIntervalType {
    Date,
    Datetime,
}

impl Display for SQLiteIntervalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SQLiteIntervalType::Date => write!(f, "date"),
            SQLiteIntervalType::Datetime => write!(f, "datetime"),
        }
    }
}

type IntervalSetter = fn(IntervalParts, i64) -> IntervalParts;

impl IntervalParts {
    fn new() -> Self {
        Self::default()
    }

    fn intraday(&self) -> bool {
        self.hours > 0 || self.minutes > 0 || self.seconds > 0 || self.nanos > 0
    }

    fn negate(mut self) -> Self {
        self.years = -self.years;
        self.months = -self.months;
        self.days = -self.days;
        self.hours = -self.hours;
        self.minutes = -self.minutes;
        self.seconds = -self.seconds;
        self
    }

    fn with_years(mut self, years: i64) -> Self {
        self.years = years;
        self
    }

    fn with_months(mut self, months: i64) -> Self {
        self.months = months;
        self
    }

    fn with_days(mut self, days: i64) -> Self {
        self.days = days;
        self
    }

    fn with_hours(mut self, hours: i64) -> Self {
        self.hours = hours;
        self
    }

    fn with_minutes(mut self, minutes: i64) -> Self {
        self.minutes = minutes;
        self
    }

    fn with_seconds(mut self, seconds: i64) -> Self {
        self.seconds = seconds;
        self
    }

    fn with_nanos(mut self, nanos: u32) -> Self {
        self.nanos = nanos;
        self
    }
}

impl VisitorMut for SQLiteVisitor {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        // for each INTERVAL, find the previous (or next, if the INTERVAL is first) expression or column name that is associated with it
        // e.g. `column_name + INTERVAL '1' DAY``, we should find the `column_name`
        // then replace the `INTERVAL` with e.g. `datetime(column_name, '+1 day')`
        // this should also apply to expressions though, like `CAST(column_name AS TEXT) + INTERVAL '1' DAY`
        // in this example, it would be replaced with `datetime(CAST(column_name AS TEXT), '+1 day')`

        // TODO: figure out nested BinaryOp, e.g. `column_name + INTERVAL '1' DAY + INTERVAL '1' DAY`
        if let Expr::BinaryOp { op, left, right } = expr {
            if *op != BinaryOperator::Plus && *op != BinaryOperator::Minus {
                return ControlFlow::Continue(());
            }

            let (target, interval) = SQLiteVisitor::normalize_interval_expr(left, right);

            if let Expr::Interval(_) = interval.as_ref() {
                // parse the INTERVAL and get the bits out of it
                // e.g. INTERVAL 0 YEARS 0 MONS 1 DAYS 0 HOURS 0 MINUTES 0.000000000 SECS -> IntervalParts { days: 1 }
                if let Ok(interval_parts) = SQLiteVisitor::parse_interval(interval) {
                    // negate the interval parts if the operator is minus
                    let interval_parts = if *op == BinaryOperator::Minus {
                        interval_parts.negate()
                    } else {
                        interval_parts
                    };

                    *expr = SQLiteVisitor::create_datetime_function(target, &interval_parts);
                }
            }
        }
        ControlFlow::Continue(())
    }
}

impl SQLiteVisitor {
    // normalize the sides of the operation to make sure the INTERVAL is always on the right
    fn normalize_interval_expr<'a>(
        left: &'a mut Box<Expr>,
        right: &'a mut Box<Expr>,
    ) -> (&'a mut Box<Expr>, &'a mut Box<Expr>) {
        if let Expr::Interval { .. } = left.as_ref() {
            (right, left)
        } else {
            (left, right)
        }
    }

    fn parse_interval(interval: &Expr) -> Result<IntervalParts, DataFusionError> {
        if let Expr::Interval(interval_expr) = interval {
            if let Expr::Value(ast::Value::SingleQuotedString(value)) = interval_expr.value.as_ref()
            {
                return SQLiteVisitor::parse_interval_string(value);
            }
        }
        Err(DataFusionError::Plan(
            "Invalid interval expression".to_string(),
        ))
    }

    fn parse_interval_string(value: &str) -> Result<IntervalParts, DataFusionError> {
        let mut parts = IntervalParts::new();
        let mut remaining = value;

        let components: [(_, IntervalSetter); 5] = [
            ("YEARS", IntervalParts::with_years),
            ("MONS", IntervalParts::with_months),
            ("DAYS", IntervalParts::with_days),
            ("HOURS", IntervalParts::with_hours),
            ("MINS", IntervalParts::with_minutes),
        ];

        for (unit, setter) in &components {
            if let Some((value, rest)) = remaining.split_once(unit) {
                let parsed_value: i64 = SQLiteVisitor::parse_value(value.trim())?;
                parts = setter(parts, parsed_value);
                remaining = rest;
            }
        }

        // Parse seconds and nanoseconds separately
        if let Some((secs, _)) = remaining.split_once("SECS") {
            let (seconds, nanos) = SQLiteVisitor::parse_seconds_and_nanos(secs.trim())?;
            parts = parts.with_seconds(seconds).with_nanos(nanos);
        }

        Ok(parts)
    }

    fn parse_seconds_and_nanos(value: &str) -> Result<(i64, u32), DataFusionError> {
        let parts: Vec<&str> = value.split('.').collect();
        let seconds = SQLiteVisitor::parse_value(parts[0])?;
        let nanos = if parts.len() > 1 {
            let nanos_str = format!("{:0<9}", parts[1]);
            nanos_str[..9].parse().map_err(|_| {
                DataFusionError::Plan(format!("Failed to parse nanoseconds: {}", parts[1]))
            })?
        } else {
            0
        };
        Ok((seconds, nanos))
    }

    fn parse_value<T: FromStr>(value: &str) -> Result<T, DataFusionError> {
        value
            .parse()
            .map_err(|_| DataFusionError::Plan(format!("Failed to parse interval value: {value}")))
    }

    fn create_datetime_function(target: &Expr, interval: &IntervalParts) -> Expr {
        let interval_date_type = if interval.intraday() {
            SQLiteIntervalType::Datetime
        } else {
            SQLiteIntervalType::Date
        };

        let function_args = vec![
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(target.clone()))),
            SQLiteVisitor::create_interval_arg("years", interval.years),
            SQLiteVisitor::create_interval_arg("months", interval.months),
            SQLiteVisitor::create_interval_arg("days", interval.days),
            SQLiteVisitor::create_interval_arg("hours", interval.hours),
            SQLiteVisitor::create_interval_arg("minutes", interval.minutes),
            SQLiteVisitor::create_interval_arg_with_fraction(
                "seconds",
                interval.seconds,
                interval.nanos,
            ),
        ]
        .into_iter()
        .flatten() // flatten the list of arguments to exclude 0 values
        .collect();

        let datetime_function = Expr::Function(ast::Function {
            name: ast::ObjectName(vec![Ident::new(interval_date_type.to_string())]),
            args: ast::FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args: function_args,
                clauses: Vec::new(),
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: Vec::new(),
            parameters: ast::FunctionArguments::None,
        });

        Expr::Cast {
            expr: Box::new(datetime_function),
            data_type: ast::DataType::Text,
            format: None,
            kind: ast::CastKind::Cast,
        }
    }

    fn create_interval_arg(unit: &str, value: i64) -> Option<FunctionArg> {
        if value == 0 {
            None
        } else {
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                ast::Value::SingleQuotedString(format!("{value:+} {unit}")),
            ))))
        }
    }

    fn create_interval_arg_with_fraction(
        unit: &str,
        value: i64,
        fraction: u32,
    ) -> Option<FunctionArg> {
        if value == 0 && fraction == 0 {
            None
        } else {
            let fraction_str = if fraction > 0 {
                format!(".{fraction:09}")
            } else {
                String::new()
            };

            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                ast::Value::SingleQuotedString(format!("{value:+}{fraction_str} {unit}")),
            ))))
        }
    }
}
