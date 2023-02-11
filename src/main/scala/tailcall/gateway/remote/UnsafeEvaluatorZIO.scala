package tailcall.gateway.remote

import zio.schema.{DynamicValue, Schema}
import zio.{Ref, Task, UIO, ZIO}

trait UnsafeEvaluatorZIO {
  final def evaluateAs[A](eval: DynamicEval): Task[A] =
    evaluate(eval).flatMap(any => ZIO.attempt(any.asInstanceOf[A]))
  def evaluate(eval: DynamicEval): Task[Any]
}

object UnsafeEvaluatorZIO {

  import DynamicEval._
  final class Default(val context: EvaluationContext) extends UnsafeEvaluatorZIO {

    def toTypedValue(value: DynamicValue, schema: Schema[_]): Task[Any] = {
      value.toTypedValue(schema) match {
        case Left(cause)  => ZIO.fail(EvaluationError.TypeError(value, cause, schema))
        case Right(value) => ZIO.succeed(value)
      }
    }

    def call[A](func: EvalFunction, arg: Any): Task[A] =
      for {
        _      <- context.set(func.input.id, arg)
        result <- evaluateAs[A](func.body)
        _      <- context.drop(func.input.id)
      } yield result

    def evaluate(eval: DynamicEval): Task[Any] =
      eval match {
        case Literal(value, meta) => ZIO
            .fromEither(value.toTypedValue(meta.toSchema))
            .mapError(cause => EvaluationError.TypeError(value, cause, meta.toSchema))

        case EqualTo(left, right, tag)   => for {
            leftValue  <- evaluate(left)
            rightValue <- evaluate(right)
          } yield tag.equal(leftValue, rightValue)
        case Math(operation, tag)        => operation match {
            case Math.Binary(left, right, operation) =>
              for {
                leftValue  <- evaluate(left)
                rightValue <- evaluate(right)
              } yield operation match {
                case Math.Binary.Add      => tag.add(leftValue, rightValue)
                case Math.Binary.Multiply => tag.multiply(leftValue, rightValue)
                case Math.Binary.Divide   => tag.divide(leftValue, rightValue)
                case Math.Binary.Modulo   => tag.modulo(leftValue, rightValue)
              }
            case Math.Unary(value, operation)        => evaluate(value)
                .map(evaluate => operation match { case Math.Unary.Negate => tag.negate(evaluate) })
          }
        case Logical(operation)          => operation match {
            case Logical.Binary(left, right, operation) =>
              for {
                leftValue  <- evaluateAs[Boolean](left)
                rightValue <- evaluateAs[Boolean](right)
              } yield operation match {
                case Logical.Binary.And => leftValue && rightValue
                case Logical.Binary.Or  => leftValue || rightValue
              }
            case Logical.Unary(value, operation)        => evaluateAs[Boolean](value).flatMap { a =>
                operation match {
                  case Logical.Unary.Not                      => ZIO.succeed(!a)
                  case Logical.Unary.Diverge(isTrue, isFalse) =>
                    if (a) evaluate(isTrue) else evaluate(isFalse)
                }
              }
          }
        case StringOperations(operation) => operation match {
            case StringOperations.Concat(left, right) => for {
                leftValue  <- evaluateAs[String](left)
                rightValue <- evaluateAs[String](right)
              } yield leftValue ++ rightValue
          }
        case SeqOperations(operation)    => operation match {
            case SeqOperations.Concat(left, right)    => for {
                leftValue  <- evaluateAs[Seq[_]](left)
                rightValue <- evaluateAs[Seq[_]](right)
              } yield leftValue ++ rightValue
            case SeqOperations.IndexOf(seq, element)  => for {
                seq <- evaluateAs[Seq[_]](seq)
                e   <- evaluate(element)
              } yield seq.indexOf(e)
            case SeqOperations.Reverse(seq)           => evaluateAs[Seq[_]](seq).map(_.reverse)
            case SeqOperations.Filter(seq, condition) => for {
                seq    <- evaluateAs[Seq[_]](seq)
                result <- ZIO.filter(seq)(any => call[Boolean](condition, any))
              } yield result

            case SeqOperations.FlatMap(seq, operation) => for {
                seq    <- evaluateAs[Seq[Any]](seq)
                result <- ZIO.foreach(seq)(any => call[Seq[_]](operation, any))
              } yield result.flatten
            case SeqOperations.Length(seq)             => evaluateAs[Seq[_]](seq).map(_.length)
            case SeqOperations.Sequence(value)         => ZIO.foreach(value)(evaluate)
          }
        case EitherOperations(operation) => operation match {
            case EitherOperations.Cons(value)              => value match {
                case Left(value)  => evaluate(value).map(Left(_))
                case Right(value) => evaluate(value).map(Right(_))
              }
            case EitherOperations.Fold(value, left, right) => for {
                either <- evaluateAs[Either[_, _]](value)
                result <- either match {
                  case Left(value)  => call[Any](left, value)
                  case Right(value) => call[Any](right, value)
                }
              } yield result
          }
        case FunctionCall(f, arg)        => evaluate(arg).flatMap(call(f, _))
        case Binding(id)                 => context.get(id)
        case EvalFunction(_, body)       => evaluate(body)
        case OptionOperations(operation) => operation match {
            case OptionOperations.Cons(option)            => option match {
                case Some(value) => evaluate(value).map(Some(_))
                case None        => ZIO.none
              }
            case OptionOperations.Fold(value, none, some) => for {
                option <- evaluateAs[Option[_]](value)
                result <- option match {
                  case Some(value) => call(some, value)
                  case None        => evaluate(none)
                }
              } yield result
          }

        case ContextOperations(_, _) => ???
        case Die(message)            => evaluateAs[String](message)
            .flatMap(message => ZIO.fail(EvaluationError.Death(message)))
      }
  }

  def make(bindings: Map[Int, Any] = Map.empty): UIO[UnsafeEvaluatorZIO] =
    for { map <- Ref.make(bindings) } yield new Default(EvaluationContext(map))

  final case class EvaluationContext(map: Ref[Map[Int, Any]]) {
    def get(id: Int): Task[Any] =
      map
        .get
        .flatMap { map =>
          map.get(id) match {
            case None        => ZIO.fail(EvaluationError.BindingNotFound(id))
            case Some(value) => ZIO.succeed(value)
          }
        }

    def set(id: Int, value: Any): Task[Unit] = map.update(_ + (id -> value))

    def drop(id: Int): Task[Unit] = map.update(_ - id)
  }
}
